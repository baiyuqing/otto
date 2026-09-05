package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"reflect"
	"sort"
	"strconv"
	"strings"
	"time"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/auth"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/memory"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/provider/openaicompat"
	"github.com/baiyuqing/otto/internal/provider/openairesponses"
	"github.com/baiyuqing/otto/internal/safetext"
	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/skill"
	"github.com/baiyuqing/otto/internal/subagent"
	"github.com/baiyuqing/otto/internal/tool"
	"github.com/baiyuqing/otto/internal/trace"
	"github.com/baiyuqing/otto/internal/urlprivacy"
)

var errSessionOperationUnavailable = errors.New("session operation is unavailable")

type preparedSession interface {
	Info() session.SessionInfo
	Activate(context.Context) (session.Session, []session.Warning, error)
	Close() error
}

type preparedStore struct {
	prepared *session.Prepared
}

type capturedAuthCredentials struct {
	path            string
	credentials     auth.Credentials
	loaded          bool
	complete        bool
	redactionValues []string
}

func (p *preparedStore) Info() session.SessionInfo {
	return p.prepared.Info()
}

func (p *preparedStore) Activate(ctx context.Context) (session.Session, []session.Warning, error) {
	store, warnings, err := p.prepared.Activate(ctx)
	if store == nil {
		return nil, warnings, err
	}
	return store, warnings, err
}

func (p *preparedStore) Close() error {
	return p.prepared.Close()
}

type runtimeBuilder struct {
	configPath              string
	config                  config.File
	environment             map[string]string
	workspace               *tool.Workspace
	workspacePath           string
	sessionRoot             string
	shell                   string
	noSession               bool
	stderr                  io.Writer
	deps                    runDependencies
	prepareSession          func(context.Context, string, string) (preparedSession, error)
	prepareListedSession    func(context.Context, string, string, string) (preparedSession, error)
	buildRunnerOverride     func(session.Session, config.Runtime) (app.Runner, error)
	setDefaultProfile       func(context.Context, string) error
	runtimeOverrides        config.Overrides
	commandExecutor         sandbox.CommandExecutor
	sandboxEnvironment      []string
	sandboxInfo             app.SandboxInfo
	sandboxSecrets          []string
	sandboxSecretsComplete  bool
	authPath                string
	authCredentials         auth.Credentials
	authCredentialsLoaded   bool
	memoryService           memory.Service
	memoryUsable            bool
	memoryUserScope         memory.Scope
	memoryWorkspaceScope    memory.Scope
	memoryRecallLimit       int
	memoryRecallTokenBudget int
	// traceWriter is non-nil only when OTTO_TRACE is set; the provider's
	// HTTP transport is wrapped to append safe request/response metadata to it.
	traceWriter io.Writer
	// extraTools is test-only: appended before registry construction so
	// tests can force tool.NewRegistry to fail (e.g. a duplicate name) and
	// exercise the memory-binding cleanup path deterministically.
	extraTools []tool.Tool
}

func newRuntimeBuilder(configPath string, configFile config.File, environment map[string]string, workspace *tool.Workspace, workspacePath, sessionRoot, shell string, options cliOptions, stderr io.Writer, deps runDependencies) runtimeBuilder {
	builder := runtimeBuilder{
		configPath:             configPath,
		config:                 configFile,
		environment:            environment,
		workspace:              workspace,
		workspacePath:          workspacePath,
		sessionRoot:            sessionRoot,
		shell:                  shell,
		noSession:              options.noSession,
		stderr:                 stderr,
		deps:                   deps,
		prepareSession:         deps.prepareSession,
		prepareListedSession:   deps.prepareListedSession,
		sandboxSecretsComplete: true,
		runtimeOverrides: config.Overrides{
			BaseURL:        options.baseURL,
			Thinking:       options.thinking,
			ShellTimeout:   options.shellTimeout,
			MaxOutputBytes: options.maxOutput,
		},
	}
	builder.traceWriter = openTraceWriter(environment["OTTO_TRACE"], sessionRoot, stderr)
	if builder.prepareSession == nil {
		builder.prepareSession = prepareSession
	}
	if builder.buildRunnerOverride == nil && deps.newRunner != nil {
		builder.buildRunnerOverride = func(current session.Session, _ config.Runtime) (app.Runner, error) {
			return deps.newRunner(current), nil
		}
	}
	return builder
}

func captureAuthCredentials(path string) capturedAuthCredentials {
	capture := capturedAuthCredentials{path: path, complete: true}
	if path == "" {
		capture.complete = false
		return capture
	}
	creds, err := auth.Load(path)
	if err != nil {
		if errors.Is(err, auth.ErrNoCredentials) {
			return capture
		}
		capture.complete = false
		return capture
	}
	redactions := safetext.NewSecretCollector()
	exact := safetext.NewSecretCollector()
	for _, value := range []string{creds.AccessToken, creds.RefreshToken, creds.IDToken, creds.AccountID} {
		if value == "" {
			continue
		}
		if !redactions.Add(value) || !exact.AddForm(value) || len(value) > memory.MaxExactGuardValueBytes {
			capture.complete = false
			return capture
		}
	}
	redactionValues := redactions.Values()
	if _, ok := safetext.DynamicRedactionMarker(redactionValues); !ok {
		capture.complete = false
		return capture
	}
	capture.credentials = creds
	capture.loaded = true
	capture.redactionValues = redactionValues
	return capture
}

func (b runtimeBuilder) resolveSession(metadata session.RuntimeMetadata) (config.Runtime, error) {
	runtime, err := resolveSessionRuntime(b.config, b.resumeEnvironment(), metadata, b.runtimeOverrides)
	if err != nil {
		return config.Runtime{}, b.redactLocalError(err, nil)
	}
	return runtime, nil
}

func (b runtimeBuilder) buildRunner(ctx context.Context, current session.Session, runtime config.Runtime) (app.Runner, error) {
	if b.buildRunnerOverride != nil {
		runner, err := b.buildRunnerOverride(current, runtime)
		if err != nil {
			return nil, b.redactError(err, &runtime)
		}
		return runner, nil
	}
	if b.workspace == nil {
		return nil, errors.New("workspace is required")
	}
	redactionValues := b.secretValues(&runtime)
	tools := b.builtinFileTools(runtime.MaxOutputBytes)
	if b.bashConfigured() {
		bash, err := tool.NewBashTool(
			b.workspace,
			b.commandExecutor,
			b.shell,
			cloneSandboxRuntimeStrings(b.sandboxEnvironment),
			runtime.ShellTimeout,
			runtime.MaxOutputBytes,
			cloneSandboxRuntimeStrings(redactionValues),
		)
		if err != nil {
			return nil, fmt.Errorf("create bash tool: %w", err)
		}
		tools = append(tools, bash)
	}
	var binding memory.Binding
	if b.memoryUsable && b.boundaryAllowsDynamic(&runtime) {
		tools = append(tools, b.memoryTools(runtime.MaxOutputBytes)...)
		memoryScopes := []memory.Scope{b.memoryUserScope, b.memoryWorkspaceScope}
		bound, err := b.memoryService.Bind(ctx, memory.BindOptions{
			Scopes:            memoryScopes,
			DefaultWriteScope: b.memoryWorkspaceScope,
		})
		if err != nil {
			return nil, fmt.Errorf("bind memory: %w", err)
		}
		binding = bound
	}
	skills := config.ResolveSkills(b.config, b.environment, b.workspacePath)
	catalog, skillWarnings := skill.Discover(skills.Roots)
	skillSection, sectionWarnings := skill.PromptSection(catalog)
	for _, warning := range append(skillWarnings, sectionWarnings...) {
		if b.stderr != nil {
			fmt.Fprintf(b.stderr, "warning: %s\n", warning)
		}
	}
	if catalog.Len() > 0 {
		tools = append(tools, tool.NewSkillTool(catalog, runtime.MaxOutputBytes))
	}

	agents, agentsErr := config.ResolveAgents(b.config, b.environment, b.workspacePath)
	if agentsErr != nil {
		if binding != nil {
			_ = binding.Close()
		}
		return nil, agentsErr
	}
	var agentCatalog subagent.Catalog
	var agentWarnings []string
	if agents.Enabled {
		agentCatalog, agentWarnings = subagent.Discover(agents.Roots)
	}
	agentSection, agentSectionWarnings := subagent.PromptSection(agentCatalog)
	for _, warning := range append(agentWarnings, agentSectionWarnings...) {
		if b.stderr != nil {
			fmt.Fprintf(b.stderr, "warning: %s\n", warning)
		}
	}

	tools = append(tools, b.extraTools...)

	redactor := b.boundaryRedactor(&runtime)
	var client provider.Provider
	var err error
	if b.boundaryAllowsDynamic(&runtime) {
		client, err = b.buildProvider(ctx, runtime)
		if err != nil {
			if binding != nil {
				_ = binding.Close()
			}
			return nil, b.redactError(err, &runtime)
		}
	}

	// promptTail is the redacted workspace-context-and-skills suffix shared
	// by the parent's final system prompt and the sub-agent runner's
	// PromptFor closure, computed once so both stay in sync. The Agents
	// section is parent-only: children never have the agent tool.
	var contextExecutor sandbox.CommandExecutor
	if redactor.AllowsDynamicContent() && b.bashConfigured() {
		contextExecutor = b.commandExecutor
	}
	promptTail := redactor.RedactString(workspaceContextFor(b.workspacePath, time.Now(), contextExecutor, b.sandboxEnvironment, b.workspace) + skillSection)
	parentAgentSection := redactor.RedactString(agentSection)
	endpointHost := endpointHostFor(runtime.BaseURL)
	compaction := agent.CompactionSettings{
		Auto:             runtime.Compaction.Auto,
		HardInputWindow:  runtime.Compaction.HardInputWindow,
		WorkingWindow:    runtime.Compaction.WorkingWindow,
		ReserveTokens:    runtime.Compaction.ReserveTokens,
		KeepRecentTokens: runtime.Compaction.KeepRecentTokens,
	}

	var tasks *agent.Tasks
	var inbox *agent.Inbox
	if client != nil && agents.Enabled {
		tasks = agent.NewTasks()
		subagentRunner, runnerWarnings, subagentErr := subagent.NewRunner(subagent.Config{
			Provider: client, Tools: tools, Redactor: redactor,
			Options: agent.Options{Model: runtime.Model, Thinking: runtime.Thinking, Compaction: compaction},
			PromptFor: func(defs []model.ToolDefinition) string {
				return systemPromptFor(defs, b.effectiveSandboxInfo(), runtime.Provider, endpointHost, runtime.Model) + promptTail
			},
			Header:         current.Header(),
			Tasks:          tasks,
			Catalog:        agentCatalog,
			ParentSession:  current.Messages,
			MaxParallel:    agents.MaxParallel,
			MaxOutputBytes: runtime.MaxOutputBytes,
		})
		if subagentErr != nil {
			if binding != nil {
				_ = binding.Close()
			}
			return nil, fmt.Errorf("create sub-agent runner: %w", subagentErr)
		}
		for _, warning := range runnerWarnings {
			if b.stderr != nil {
				fmt.Fprintf(b.stderr, "warning: %s\n", warning)
			}
		}
		tools = append(tools, subagentRunner.Tools()...)
		inbox = tasks.Notifications()
	}

	registry, err := tool.NewRegistry(tools...)
	if err != nil {
		if binding != nil {
			_ = binding.Close()
		}
		return nil, fmt.Errorf("create tool registry: %w", err)
	}
	systemPrompt := systemPromptFor(registry.Definitions(), b.effectiveSandboxInfo(), runtime.Provider, endpointHost, runtime.Model) + promptTail + parentAgentSection
	return agent.New(client, registry, current, agent.Options{
		Model: runtime.Model, SystemPrompt: systemPrompt, Thinking: runtime.Thinking,
		Compaction:              compaction,
		Memory:                  binding,
		MemoryRecallLimit:       b.memoryRecallLimit,
		MemoryRecallTokenBudget: b.memoryRecallTokenBudget,
		Tasks:                   tasks,
		Inbox:                   inbox,
	}, redactor), nil
}

func (b runtimeBuilder) bashConfigured() bool {
	return b.boundaryAllowsDynamic(nil) && b.plannedBashAvailable()
}

func (b runtimeBuilder) plannedBashAvailable() bool {
	return b.sandboxInfo.BashAvailable && b.sandboxEnvironment != nil && !isNilSandboxRuntimeValue(b.commandExecutor)
}

func (b runtimeBuilder) plannedSandboxInfo() app.SandboxInfo {
	if b.sandboxInfo.BashAvailable && !b.plannedBashAvailable() {
		return app.SandboxInfo{Mode: app.SandboxUnavailable, BashAvailable: false, Reason: app.SandboxReasonRuntimeFailure}
	}
	return b.sandboxInfo
}

func (b runtimeBuilder) effectiveSandboxInfo() app.SandboxInfo {
	if !b.boundaryAllowsDynamic(nil) {
		return app.SandboxInfo{Mode: app.SandboxUnavailable, BashAvailable: false, Reason: app.SandboxReasonEnvironmentRejected}
	}
	return b.plannedSandboxInfo()
}

func (b runtimeBuilder) runtimeInfo(runtime config.Runtime) app.RuntimeInfo {
	info := app.RuntimeInfo{
		Provider: runtime.Provider, Profile: runtime.Profile, Model: runtime.Model,
		ContextWindow: runtime.Compaction.ContextWindow, Sandbox: b.effectiveSandboxInfo(),
	}
	if !b.boundaryAllowsDynamic(&runtime) {
		info.Provider = ""
		info.Profile = ""
		info.Model = ""
		info.ContextWindow = 0
	}
	return info
}

func (b runtimeBuilder) buildProvider(ctx context.Context, runtime config.Runtime) (provider.Provider, error) {
	if runtime.Provider != config.ProviderChatGPT {
		return openaicompat.New(runtime.BaseURL, runtime.APIKey, b.tracingHTTPClient(openaicompat.DefaultHTTPClient())), nil
	}
	if !b.authCredentialsLoaded {
		return nil, auth.ErrNoCredentials
	}
	path := strings.TrimSpace(b.authPath)
	if path == "" {
		return nil, auth.ErrCredentialsUnavailable
	}
	creds := b.authCredentials
	return openairesponses.New(creds.TokenSource(ctx, path), creds.AccountID, b.tracingHTTPClient(openairesponses.DefaultHTTPClient())), nil
}

// tracingHTTPClient wraps a hardened provider client to record safe HTTP metadata,
// or returns nil when tracing is off (providers build their own hardened
// defaults for nil). Wrapping each provider's default transport preserves its
// tuned timeouts and redirect policy.
func (b runtimeBuilder) tracingHTTPClient(client *http.Client) *http.Client {
	if b.traceWriter == nil {
		return nil
	}
	if client == nil {
		client = http.DefaultClient
	}
	client.Transport = trace.NewRoundTripper(client.Transport, b.traceWriter)
	return client
}

// openTraceWriter resolves OTTO_TRACE into an append writer for safe provider
// HTTP records, or nil when tracing is off. Empty disables it; "1"/"true"/"on"
// writes to ~/.otto/traces/<timestamp>-<pid>.jsonl; any other value is a file
// path. Failures degrade to no tracing with a stderr warning — this is a
// development aid, not a required feature.
func openTraceWriter(value, sessionRoot string, stderr io.Writer) io.Writer {
	value = strings.TrimSpace(value)
	if value == "" {
		return nil
	}
	path := value
	switch strings.ToLower(value) {
	case "1", "true", "on", "yes":
		dir := filepath.Join(filepath.Dir(sessionRoot), "traces")
		name := fmt.Sprintf("%s-%d.jsonl", time.Now().UTC().Format("20060102T150405"), os.Getpid())
		path = filepath.Join(dir, name)
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		fmt.Fprintf(stderr, "otto: OTTO_TRACE disabled: %v\n", err)
		return nil
	}
	file, err := os.OpenFile(path, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	if err != nil {
		fmt.Fprintf(stderr, "otto: OTTO_TRACE disabled: %v\n", err)
		return nil
	}
	fmt.Fprintf(stderr, "otto: tracing provider HTTP to %s\n", path)
	return file
}

func (b runtimeBuilder) buildNewReplacement(ctx context.Context, current app.RuntimeInfo) (app.SessionReplacement, error) {
	if err := ctx.Err(); err != nil {
		return app.SessionReplacement{}, err
	}
	if !b.boundaryAllowsDynamic(nil) {
		return app.SessionReplacement{}, errSessionOperationUnavailable
	}
	runtime, err := b.resolveSession(session.RuntimeMetadata{
		Profile: current.Profile, Provider: current.Provider, Model: current.Model,
	})
	if err != nil {
		return app.SessionReplacement{}, err
	}
	if !b.boundaryAllowsDynamic(&runtime) {
		return app.SessionReplacement{}, errSessionOperationUnavailable
	}
	return b.freshReplacement(ctx, runtime)
}

// buildProfileReplacement resolves the named profile as an explicit override so
// its own provider/model/base_url win (matching startup --profile), then builds
// a fresh session on it. Switching profile therefore also switches provider.
// An unknown profile is rejected by config.Resolve before any session is created.
func (b runtimeBuilder) buildProfileReplacement(ctx context.Context, profile string) (app.SessionReplacement, error) {
	if err := ctx.Err(); err != nil {
		return app.SessionReplacement{}, err
	}
	overrides := b.runtimeOverrides
	overrides.Profile = profile
	overrides.Provider = ""
	overrides.BaseURL = ""
	overrides.Model = ""
	runtime, err := config.Resolve(b.config, b.resumeEnvironment(), config.SessionDefaults{}, overrides)
	if err != nil {
		return app.SessionReplacement{}, b.redactError(err, nil)
	}
	if !b.boundaryAllowsDynamic(&runtime) {
		return app.SessionReplacement{}, errSessionOperationUnavailable
	}
	return b.freshReplacement(ctx, runtime)
}

func (b runtimeBuilder) persistDefaultProfile(ctx context.Context, profile string) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	if b.setDefaultProfile != nil {
		return b.setDefaultProfile(ctx, profile)
	}
	return config.SetDefaultProfile(b.configPath, profile)
}

// profileNames returns the configured profile names in sorted order for
// display by the /model command.
func (b runtimeBuilder) profileNames() []string {
	names := make([]string, 0, len(b.config.Profiles))
	for name := range b.config.Profiles {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}

// freshReplacement creates a new session and runner for an already-resolved
// runtime, shared by the /new and /model replacement paths.
func (b runtimeBuilder) freshReplacement(ctx context.Context, runtime config.Runtime) (app.SessionReplacement, error) {
	create := b.deps.newSession
	if create == nil {
		create = newSession
	}
	candidate, err := create(b.noSession, b.sessionRoot, b.workspacePath, runtime)
	if err != nil {
		return app.SessionReplacement{}, b.cleanupCandidate(candidate, err, &runtime)
	}
	if candidate == nil {
		return app.SessionReplacement{}, b.redactError(errors.New("session factory returned nil session"), &runtime)
	}
	if err := ctx.Err(); err != nil {
		return app.SessionReplacement{}, b.cleanupCandidate(candidate, err, &runtime)
	}

	runner, err := b.buildRunner(ctx, candidate, runtime)
	if err != nil {
		return app.SessionReplacement{}, b.cleanupCandidate(candidate, err, &runtime)
	}
	if runner == nil {
		return app.SessionReplacement{}, b.cleanupReplacement(candidate, runner, errors.New("runner factory returned nil runner"), &runtime)
	}
	if err := ctx.Err(); err != nil {
		return app.SessionReplacement{}, b.cleanupReplacement(candidate, runner, err, &runtime)
	}
	if err := b.updateSessionRuntime(ctx, candidate, runtime); err != nil {
		return app.SessionReplacement{}, b.cleanupReplacement(candidate, runner, err, &runtime)
	}
	return app.SessionReplacement{
		Session: candidate, Runner: runner, RuntimeInfo: b.runtimeInfo(runtime),
	}, nil
}

func (b runtimeBuilder) openReplacement(ctx context.Context, path string) (app.SessionReplacement, error) {
	if err := ctx.Err(); err != nil {
		return app.SessionReplacement{}, err
	}
	if !b.boundaryAllowsDynamic(nil) {
		return app.SessionReplacement{}, errSessionOperationUnavailable
	}
	prepared, err := b.prepareListed(ctx, path)
	if err != nil {
		return app.SessionReplacement{}, b.redactError(err, nil)
	}
	defer prepared.Close()

	info := prepared.Info()
	runtime, err := b.resolveSession(session.RuntimeMetadata{
		Profile:  info.Profile,
		Provider: info.Provider,
		Model:    info.Model,
	})
	if err != nil {
		return app.SessionReplacement{}, err
	}
	if !b.boundaryAllowsDynamic(&runtime) {
		return app.SessionReplacement{}, errSessionOperationUnavailable
	}
	candidate, warnings, err := b.activatePrepared(ctx, prepared, info, &runtime)
	if err != nil {
		return app.SessionReplacement{}, err
	}
	runner, err := b.buildRunner(ctx, candidate, runtime)
	if err != nil {
		return app.SessionReplacement{}, b.cleanupCandidate(candidate, err, &runtime)
	}
	if runner == nil {
		return app.SessionReplacement{}, b.cleanupReplacement(candidate, runner, errors.New("runner factory returned nil runner"), &runtime)
	}
	if err := ctx.Err(); err != nil {
		return app.SessionReplacement{}, b.cleanupReplacement(candidate, runner, err, &runtime)
	}
	if err := b.updateSessionRuntime(ctx, candidate, runtime); err != nil {
		return app.SessionReplacement{}, b.cleanupReplacement(candidate, runner, err, &runtime)
	}
	return app.SessionReplacement{
		Session: candidate, Runner: runner, RuntimeInfo: b.runtimeInfo(runtime), Warnings: cloneWarnings(warnings),
	}, nil
}

func (b runtimeBuilder) prepare(ctx context.Context, path string) (preparedSession, error) {
	prepare := b.prepareSession
	if prepare == nil {
		prepare = prepareSession
	}
	return checkedPreparedSession(prepare(ctx, path, b.workspacePath))
}

func (b runtimeBuilder) prepareListed(ctx context.Context, path string) (preparedSession, error) {
	if b.prepareListedSession != nil {
		return checkedPreparedSession(b.prepareListedSession(ctx, b.sessionRoot, b.workspacePath, path))
	}
	// Preserve the narrow injected preparation seam used by construction tests.
	// Production builders always install the restricted listed preparer.
	if b.prepareSession != nil {
		return checkedPreparedSession(b.prepareSession(ctx, path, b.workspacePath))
	}
	return checkedPreparedSession(prepareListedSession(ctx, b.sessionRoot, b.workspacePath, path))
}

func checkedPreparedSession(prepared preparedSession, err error) (preparedSession, error) {
	if err != nil {
		if prepared != nil {
			if closeErr := prepared.Close(); closeErr != nil {
				err = errors.Join(err, closeErr)
			}
		}
		return nil, err
	}
	if prepared == nil {
		return nil, errors.New("session prepare returned nil handle")
	}
	return prepared, nil
}

func (b runtimeBuilder) activatePrepared(ctx context.Context, prepared preparedSession, info session.SessionInfo, runtime *config.Runtime) (session.Session, []session.Warning, error) {
	candidate, warnings, err := prepared.Activate(ctx)
	if err != nil {
		return nil, nil, b.cleanupCandidate(candidate, err, runtime)
	}
	if candidate == nil {
		return nil, nil, b.redactError(errors.New("session activation returned nil session"), runtime)
	}
	if !activatedSessionMatchesPrepared(info, candidate.Header()) {
		return nil, nil, b.cleanupCandidate(candidate, fmt.Errorf("%w: prepared session metadata changed during activation", session.ErrInvalidSession), runtime)
	}
	return candidate, b.redactWarnings(warnings, runtime), nil
}

func (b runtimeBuilder) redactWarnings(warnings []session.Warning, runtime *config.Runtime) []session.Warning {
	redacted := cloneWarnings(warnings)
	boundary := b.boundaryRedactor(runtime)
	for index := range redacted {
		redacted[index].Message = boundary.RedactString(redacted[index].Message)
	}
	return redacted
}

func activatedSessionMatchesPrepared(info session.SessionInfo, header session.Header) bool {
	return info.ID == header.ID &&
		info.CWD == header.Workspace &&
		info.Profile == header.Profile &&
		info.Provider == header.Provider &&
		info.Model == header.Model
}

func (b runtimeBuilder) updateSessionRuntime(ctx context.Context, current session.Session, runtime config.Runtime) error {
	if !b.boundaryAllowsDynamic(&runtime) {
		return nil
	}
	return updateSessionRuntime(ctx, current, runtime)
}

func updateSessionRuntime(ctx context.Context, current session.Session, runtime config.Runtime) error {
	header := current.Header()
	if header.Profile == runtime.Profile && header.Provider == runtime.Provider && header.Model == runtime.Model {
		return nil
	}
	updater, ok := current.(session.RuntimeUpdater)
	if !ok {
		return errors.New("session does not support runtime provenance updates")
	}
	return updater.UpdateRuntime(ctx, session.RuntimeMetadata{
		Profile: runtime.Profile, Provider: runtime.Provider, Model: runtime.Model,
	})
}

// newController assembles a Controller from an already-built initial session
// and runner, wiring the profile/new-session/browser/archiver/memory options
// shared by every frontend and by the agent server's per-session factories.
func (b runtimeBuilder) newController(initial app.SessionReplacement, dynamicContent bool) (*app.Controller, error) {
	options := []app.Option{
		app.WithDynamicContent(dynamicContent),
		app.WithProfileSwitcher(b.profileNames(), b.buildProfileReplacement),
		app.WithDefaultProfileSetter(b.persistDefaultProfile),
		app.WithNewSessionBuilder(b.buildNewReplacement),
	}
	if b.authPath != "" {
		options = append(options, app.WithAuthentication(auth.NewService(b.authPath)))
	}
	if b.memoryService != nil {
		options = append(options, app.WithMemory(b.memoryService, b.memoryUserScope, b.memoryWorkspaceScope))
	}
	if !b.noSession {
		options = append(options,
			app.WithSessionBrowser(func(ctx context.Context, limit int) (session.ListResult, error) {
				return session.List(ctx, b.sessionRoot, b.workspacePath, "", limit)
			}, b.openReplacement),
			app.WithSessionArchiver(func(ctx context.Context, path string) (session.ArchiveResult, error) {
				return session.Archive(ctx, b.sessionRoot, b.workspacePath, path)
			}),
		)
	}
	return b.deps.newController(initial, options...)
}

func closeRuntimeRunner(runner app.Runner) error {
	if closer, ok := runner.(io.Closer); ok {
		return closer.Close()
	}
	return nil
}

func (b runtimeBuilder) cleanupCandidate(candidate session.Session, err error, runtime *config.Runtime) error {
	return b.cleanupReplacement(candidate, nil, err, runtime)
}

func (b runtimeBuilder) cleanupReplacement(candidate session.Session, runner app.Runner, err error, runtime *config.Runtime) error {
	if candidate == nil && runner == nil {
		return b.redactError(err, runtime)
	}
	cause := err
	if candidate != nil {
		if closeErr := candidate.Close(); closeErr != nil {
			err = errors.Join(err, closeErr)
		}
	}
	if closeErr := closeRuntimeRunner(runner); closeErr != nil {
		err = errors.Join(err, closeErr)
	}
	switch cause {
	case context.Canceled, context.DeadlineExceeded:
		message, ok := b.redactedErrorMessage(err, runtime)
		if !ok {
			return cause
		}
		if message == err.Error() {
			return err
		}
		return redactedIdentityError{message: message, cause: err}
	default:
		return b.redactError(err, runtime)
	}
}

func (b runtimeBuilder) resumeEnvironment() map[string]string {
	if len(b.environment) == 0 {
		return nil
	}
	environment := make(map[string]string, len(b.environment))
	for key, value := range b.environment {
		switch key {
		case "OTTO_PROVIDER", "OTTO_PROFILE", "OTTO_MODEL", "OTTO_UI":
			continue
		default:
			environment[key] = value
		}
	}
	return environment
}

func (b runtimeBuilder) redactError(err error, runtime *config.Runtime) error {
	if err == nil {
		return nil
	}
	message, ok := b.redactedErrorMessage(err, runtime)
	if !ok {
		switch err {
		case context.Canceled, context.DeadlineExceeded:
			return err
		default:
			return errRedactedRuntimeBoundary
		}
	}
	if message == err.Error() {
		return err
	}
	switch err {
	case context.Canceled, context.DeadlineExceeded:
		return redactedIdentityError{message: message, cause: err}
	default:
		return errors.New(message)
	}
}

func (b runtimeBuilder) redactLocalError(err error, runtime *config.Runtime) error {
	if err == nil {
		return nil
	}
	message, ok := b.redactedErrorMessage(err, runtime)
	if !ok || message == err.Error() {
		return err
	}
	return errors.New(message)
}

func (b runtimeBuilder) redactedErrorMessage(err error, runtime *config.Runtime) (string, bool) {
	if err == nil {
		return "", true
	}
	values, complete := b.boundarySecretValues(runtime)
	if !complete {
		return "", false
	}
	marker, dynamic := safetext.DynamicRedactionMarker(values)
	if !dynamic {
		return "", false
	}
	message := safetext.CanonicalizeUTF8(err.Error())
	for _, value := range values {
		if value == "" {
			continue
		}
		message = strings.ReplaceAll(message, value, marker)
	}
	return message, true
}

type redactedIdentityError struct {
	message string
	cause   error
}

var errRedactedRuntimeBoundary = errors.New("")

func (e redactedIdentityError) Error() string {
	return e.message
}

func (e redactedIdentityError) Is(target error) bool {
	return errors.Is(e.cause, target)
}

func (b runtimeBuilder) secretRedactor(runtime *config.Runtime) *agent.Redactor {
	values, complete := b.boundarySecretValues(runtime)
	return agent.NewRedactorWithCompleteness(values, complete)
}

func (b runtimeBuilder) boundaryRedactor(runtime *config.Runtime) *agent.Redactor {
	boundary := b.secretRedactor(runtime)
	if !boundary.AllowsDynamicContent() || !b.boundaryFieldsUnchanged(boundary, runtime) {
		return agent.NewRedactorWithCompleteness(nil, false)
	}
	return boundary
}

func (b runtimeBuilder) secretValues(runtime *config.Runtime) []string {
	values, _ := b.boundarySecretValues(runtime)
	return values
}

func collectSecretValuesWithAuth(cfg config.File, environment map[string]string, runtime *config.Runtime, captured *auth.Credentials) []string {
	seen := make(map[string]struct{})
	values := make([]string, 0, len(cfg.Profiles)+12)
	add := func(value string) bool {
		if value == "" {
			return true
		}
		if _, ok := seen[value]; ok {
			return true
		}
		seen[value] = struct{}{}
		values = append(values, value)
		return true
	}
	add(environment["OTTO_API_KEY"])
	for _, name := range sortedProfileNames(cfg.Profiles) {
		profile := cfg.Profiles[name]
		if profile.APIKeyEnv != "" {
			add(environment[profile.APIKeyEnv])
		}
		collectURLSecretValues(profile.BaseURL, add)
	}
	if runtime != nil {
		add(runtime.APIKey)
		collectURLSecretValues(runtime.BaseURL, add)
	}
	if captured != nil {
		add(captured.AccessToken)
		add(captured.RefreshToken)
		add(captured.IDToken)
		add(captured.AccountID)
	}
	return values
}

func (b runtimeBuilder) boundaryAllowsDynamic(runtime *config.Runtime) bool {
	return b.boundaryRedactor(runtime).AllowsDynamicContent()
}

func (b runtimeBuilder) boundarySecretValues(runtime *config.Runtime) ([]string, bool) {
	collector := safetext.NewSecretCollector()
	complete := b.sandboxSecretsComplete
	collectorOpen := true
	add := func(value string) bool {
		if !collectorOpen {
			return false
		}
		if !collector.Add(value) {
			collectorOpen = false
			complete = false
			return false
		}
		return true
	}
	addURL := func(raw string) {
		if !collectURLSecretValues(raw, add) {
			complete = false
		}
	}
	for _, value := range b.sandboxSecrets {
		if !add(value) {
			return collector.Values(), false
		}
	}
	if !add("OTTO_API_KEY") || !add(b.environment["OTTO_API_KEY"]) {
		return collector.Values(), false
	}
	for _, name := range sortedProfileNames(b.config.Profiles) {
		profile := b.config.Profiles[name]
		if profile.APIKeyEnv != "" && (!add(profile.APIKeyEnv) || !add(b.environment[profile.APIKeyEnv])) {
			return collector.Values(), false
		}
		addURL(profile.BaseURL)
		if !collectorOpen {
			return collector.Values(), false
		}
	}
	addURL(b.runtimeOverrides.BaseURL)
	if !collectorOpen {
		return collector.Values(), false
	}
	for _, value := range []string{b.authCredentials.AccessToken, b.authCredentials.RefreshToken, b.authCredentials.IDToken, b.authCredentials.AccountID} {
		if value != "" && !add(value) {
			return collector.Values(), false
		}
	}
	if runtime != nil {
		if !add(runtime.APIKeyEnv) || !add(runtime.APIKey) {
			return collector.Values(), false
		}
		addURL(runtime.BaseURL)
	}
	return collector.Values(), complete
}

func sortedProfileNames(profiles map[string]config.Profile) []string {
	names := make([]string, 0, len(profiles))
	for name := range profiles {
		names = append(names, name)
	}
	sort.Strings(names)
	return names
}

func collectURLSecretValues(raw string, add func(string) bool) bool {
	if raw == "" || add == nil {
		return true
	}
	if !add(raw) {
		return false
	}
	complete := collectRawURLSecretValues(raw, add)

	parsed, err := url.Parse(raw)
	if err != nil {
		return false
	}
	if parsed.User != nil {
		username := parsed.User.Username()
		if !add(username) {
			return false
		}
		decodedUserinfo := username
		if password, ok := parsed.User.Password(); ok {
			if !add(password) {
				return false
			}
			decodedUserinfo += ":" + password
		}
		if !add(decodedUserinfo) || !add(parsed.User.String()) {
			return false
		}
	}
	return complete
}

func collectRawURLSecretValues(raw string, add func(string) bool) bool {
	userinfo, ambiguous := urlprivacy.UserinfoForms(raw)
	for _, value := range userinfo {
		if !add(value) {
			return false
		}
	}

	queryStart := strings.IndexByte(raw, '?')
	fragmentStart := strings.IndexByte(raw, '#')
	if queryStart >= 0 && (fragmentStart < 0 || queryStart < fragmentStart) {
		queryEnd := len(raw)
		if fragmentStart >= 0 {
			queryEnd = fragmentStart
		}
		for _, item := range strings.FieldsFunc(raw[queryStart+1:queryEnd], func(character rune) bool {
			return character == '&' || character == ';'
		}) {
			_, value, found := strings.Cut(item, "=")
			if !found {
				continue
			}
			if !add(value) {
				return false
			}
			if decoded, err := url.QueryUnescape(value); err == nil && !add(decoded) {
				return false
			}
		}
	}
	if fragmentStart >= 0 {
		rawFragment := raw[fragmentStart+1:]
		if !add(rawFragment) {
			return false
		}
		if decoded, err := url.PathUnescape(rawFragment); err == nil && !add(decoded) {
			return false
		}
		if decoded, err := url.QueryUnescape(rawFragment); err == nil && !add(decoded) {
			return false
		}
	}
	if len(raw) > safetext.MaxSecretBytes {
		return false
	}
	return !ambiguous
}

func (b runtimeBuilder) boundaryFieldsUnchanged(redactor *agent.Redactor, runtime *config.Runtime) bool {
	if redactor == nil || !redactor.AllowsDynamicContent() {
		return false
	}
	if b.workspacePath != "" && redactor.RedactString(b.workspacePath) != b.workspacePath {
		return false
	}
	var promptProvider, promptModel, endpointHost string
	if runtime != nil {
		promptProvider = runtime.Provider
		promptModel = runtime.Model
		endpointHost = endpointHostFor(runtime.BaseURL)
		for _, value := range []string{
			runtime.Provider,
			runtime.Profile,
			runtime.Model,
			runtime.Thinking,
			endpointHost,
			strconv.Itoa(runtime.Compaction.ContextWindow),
		} {
			if redactor.RedactString(value) != value {
				return false
			}
		}
	}
	if b.workspace == nil {
		return true
	}
	definitions := b.boundaryToolDefinitions(runtime)
	if !boundaryValueUnchanged(redactor, definitions) {
		return false
	}
	prompt := systemPromptFor(definitions, b.plannedSandboxInfo(), promptProvider, endpointHost, promptModel)
	return redactor.RedactString(prompt) == prompt
}

// endpointHostFor returns the host[:port] of baseURL, never its userinfo,
// path, or query (which may carry secrets); "" when baseURL is empty or
// unparsable.
func endpointHostFor(baseURL string) string {
	if baseURL == "" {
		return ""
	}
	parsed, err := url.Parse(baseURL)
	if err != nil {
		return ""
	}
	return parsed.Host
}

func (b runtimeBuilder) boundaryToolDefinitions(runtime *config.Runtime) []model.ToolDefinition {
	maxOutput := 1
	if runtime != nil && runtime.MaxOutputBytes > 0 {
		maxOutput = runtime.MaxOutputBytes
	}
	definitions := make([]model.ToolDefinition, 0, 6)
	for _, builtin := range b.builtinFileTools(maxOutput) {
		definitions = append(definitions, builtin.Definition())
	}
	if b.memoryUsable {
		for _, memoryTool := range b.memoryTools(maxOutput) {
			definitions = append(definitions, memoryTool.Definition())
		}
	}
	if b.plannedBashAvailable() {
		definitions = append(definitions, tool.BashDefinition())
	}
	if config.ResolveSkills(b.config, b.environment, b.workspacePath).Enabled {
		definitions = append(definitions, tool.NewSkillTool(skill.Catalog{}, maxOutput).Definition())
	}
	// Mirrors buildRunner's gating condition for building a provider client
	// (b.boundaryAllowsDynamic) without calling it: boundaryAllowsDynamic
	// calls boundaryRedactor, which calls boundaryFieldsUnchanged, which
	// calls this function, so calling boundaryAllowsDynamic here would
	// recurse. b.secretRedactor(runtime).AllowsDynamicContent() is the
	// non-recursive check boundaryAllowsDynamic itself starts from; every
	// caller of boundaryFieldsUnchanged (and so of this function) already
	// has that condition true, so this is equivalent in practice.
	if b.secretRedactor(runtime).AllowsDynamicContent() {
		// ResolveAgents' error is ignored here (treated as enabled): this
		// mirrors buildRunner's own recursion-avoidance rationale above, and
		// erring toward including the agent tools keeps the boundary check
		// conservative (a false "unchanged" is safe; the actual gate is
		// buildRunner's ResolveAgents call, which does surface the error).
		agentsRuntime, agentsErr := config.ResolveAgents(b.config, b.environment, b.workspacePath)
		if agentsErr != nil || agentsRuntime.Enabled {
			definitions = append(definitions, subagent.ToolDefinitions()...)
		}
	}
	return definitions
}

func (b runtimeBuilder) builtinFileTools(maxOutput int) []tool.Tool {
	return []tool.Tool{
		tool.NewReadTool(b.workspace, maxOutput),
		tool.NewGrepTool(b.workspace, maxOutput),
		tool.NewFindTool(b.workspace, maxOutput),
		tool.NewLSTool(b.workspace, maxOutput),
		tool.NewWriteTool(b.workspace),
		tool.NewEditTool(b.workspace),
	}
}

func (b runtimeBuilder) memoryTools(maxOutput int) []tool.Tool {
	return []tool.Tool{
		tool.NewMemorySearchTool(b.memoryService, []memory.Scope{b.memoryUserScope, b.memoryWorkspaceScope}, maxOutput),
		tool.NewRememberTool(b.memoryService, b.memoryWorkspaceScope),
		tool.NewForgetTool(b.memoryService, []memory.Scope{b.memoryUserScope, b.memoryWorkspaceScope}),
	}
}

func boundaryValueUnchanged(redactor *agent.Redactor, value any) bool {
	if redactor == nil || !redactor.AllowsDynamicContent() || value == nil {
		return redactor != nil && redactor.AllowsDynamicContent()
	}
	switch value := value.(type) {
	case string:
		return redactor.RedactString(value) == value
	}
	reflected := reflect.ValueOf(value)
	switch reflected.Kind() {
	case reflect.Pointer, reflect.Interface:
		if reflected.IsNil() {
			return true
		}
		return boundaryValueUnchanged(redactor, reflected.Elem().Interface())
	case reflect.Struct:
		for index := 0; index < reflected.NumField(); index++ {
			if !boundaryValueUnchanged(redactor, reflected.Field(index).Interface()) {
				return false
			}
		}
		return true
	case reflect.Slice, reflect.Array:
		for index := 0; index < reflected.Len(); index++ {
			if !boundaryValueUnchanged(redactor, reflected.Index(index).Interface()) {
				return false
			}
		}
		return true
	case reflect.Map:
		iter := reflected.MapRange()
		for iter.Next() {
			if iter.Key().Kind() == reflect.String && redactor.RedactString(iter.Key().String()) != iter.Key().String() {
				return false
			}
			if !boundaryValueUnchanged(redactor, iter.Value().Interface()) {
				return false
			}
		}
		return true
	default:
		return true
	}
}

func resolveInitialRuntime(file config.File, environment map[string]string, metadata *session.RuntimeMetadata, overrides config.Overrides) (config.Runtime, error) {
	if metadata == nil {
		return config.Resolve(file, environment, config.SessionDefaults{}, overrides)
	}
	return resolveSessionRuntime(file, environment, *metadata, overrides)
}

func resolveSessionRuntime(file config.File, environment map[string]string, metadata session.RuntimeMetadata, overrides config.Overrides) (config.Runtime, error) {
	file = configForSessionRuntime(file, metadata.Profile, overrides.Profile != "")
	return config.Resolve(file, environment, config.SessionDefaults{Provider: metadata.Provider, Model: metadata.Model}, overrides)
}

func configForSessionRuntime(file config.File, storedProfile string, explicitProfile bool) config.File {
	if explicitProfile {
		return file
	}
	copy := file
	if storedProfile != "" {
		copy.DefaultProfile = storedProfile
	}
	selectedProfile := copy.DefaultProfile
	if selectedProfile == "" {
		return copy
	}
	profile, ok := copy.Profiles[selectedProfile]
	if !ok {
		return copy
	}
	copy.Profiles = cloneProfiles(copy.Profiles)
	profile.Provider = ""
	profile.Model = ""
	copy.Profiles[selectedProfile] = profile
	return copy
}

func cloneProfiles(profiles map[string]config.Profile) map[string]config.Profile {
	if profiles == nil {
		return nil
	}
	copy := make(map[string]config.Profile, len(profiles))
	for name, profile := range profiles {
		copy[name] = profile
	}
	return copy
}

func prepareSession(ctx context.Context, path, workspace string) (preparedSession, error) {
	prepared, err := session.Prepare(ctx, path)
	if err != nil {
		return nil, err
	}
	if err := validateSessionWorkspace(prepared.Info().CWD, workspace); err != nil {
		if closeErr := prepared.Close(); closeErr != nil {
			err = errors.Join(err, closeErr)
		}
		return nil, err
	}
	return &preparedStore{prepared: prepared}, nil
}

func prepareListedSession(ctx context.Context, root, workspace, path string) (preparedSession, error) {
	prepared, err := session.PrepareListed(ctx, root, workspace, path)
	if err != nil {
		return nil, err
	}
	return &preparedStore{prepared: prepared}, nil
}

func validateSessionWorkspace(sessionWorkspace, workspace string) error {
	headerWorkspace, err := canonicalDirectory(sessionWorkspace)
	if err != nil {
		return fmt.Errorf("resolve session workspace: %w", err)
	}
	if headerWorkspace != workspace {
		return fmt.Errorf("session workspace %q does not match cwd", headerWorkspace)
	}
	return nil
}

func cloneWarnings(warnings []session.Warning) []session.Warning {
	if warnings == nil {
		return nil
	}
	return append([]session.Warning(nil), warnings...)
}

func printWarnings(stderr io.Writer, warnings []session.Warning) {
	for _, warning := range warnings {
		_, _ = fmt.Fprintf(stderr, "warning: %s\n", warning.Message)
	}
}
