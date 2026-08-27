package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net/url"
	"strings"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/provider/openaicompat"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
)

type runtimeBuilder struct {
	config              config.File
	environment         map[string]string
	workspace           *tool.Workspace
	workspacePath       string
	sessionRoot         string
	shell               string
	noSession           bool
	stderr              io.Writer
	deps                runDependencies
	openSession         func(path, workspace string) (session.Session, []session.Warning, error)
	buildRunnerOverride func(session.Session, config.Runtime) (app.Runner, error)
	runtimeOverrides    config.Overrides
}

func newRuntimeBuilder(configFile config.File, environment map[string]string, workspace *tool.Workspace, workspacePath, sessionRoot, shell string, options cliOptions, stderr io.Writer, deps runDependencies) runtimeBuilder {
	builder := runtimeBuilder{
		config:        configFile,
		environment:   environment,
		workspace:     workspace,
		workspacePath: workspacePath,
		sessionRoot:   sessionRoot,
		shell:         shell,
		noSession:     options.noSession,
		stderr:        stderr,
		deps:          deps,
		openSession:   deps.openSession,
		runtimeOverrides: config.Overrides{
			MaxTurns:       options.maxTurns,
			ShellTimeout:   options.shellTimeout,
			MaxOutputBytes: options.maxOutput,
		},
	}
	if builder.openSession == nil {
		builder.openSession = openSession
	}
	if builder.buildRunnerOverride == nil && deps.newRunner != nil {
		builder.buildRunnerOverride = func(current session.Session, _ config.Runtime) (app.Runner, error) {
			return deps.newRunner(current), nil
		}
	}
	return builder
}

func (b runtimeBuilder) resolveSession(metadata session.RuntimeMetadata) (config.Runtime, error) {
	runtime, err := resolveSessionRuntime(b.config, b.resumeEnvironment(), metadata, b.runtimeOverrides)
	if err != nil {
		return config.Runtime{}, b.redactError(err, nil)
	}
	return runtime, nil
}

func (b runtimeBuilder) buildRunner(current session.Session, runtime config.Runtime) (app.Runner, error) {
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
	registry, err := tool.NewRegistry(
		tool.NewReadTool(b.workspace, runtime.MaxOutputBytes),
		tool.NewWriteTool(b.workspace),
		tool.NewEditTool(b.workspace),
		tool.NewBashTool(b.workspace, b.shell, runtime.ShellTimeout, runtime.MaxOutputBytes, tool.BashSecurity{
			RemoveEnv:    []string{"OTTO_API_KEY", runtime.APIKeyEnv},
			RedactValues: []string{runtime.APIKey},
		}),
	)
	if err != nil {
		return nil, fmt.Errorf("create tool registry: %w", err)
	}
	client := openaicompat.New(runtime.BaseURL, runtime.APIKey, nil)
	return agent.New(client, registry, current, agent.Options{
		Model: runtime.Model, SystemPrompt: systemPrompt, MaxTurns: runtime.MaxTurns,
	}), nil
}

func (b runtimeBuilder) openReplacement(ctx context.Context, path string) (app.SessionReplacement, error) {
	info, err := inspectSession(ctx, path, b.workspacePath)
	if err != nil {
		return app.SessionReplacement{}, b.redactError(err, nil)
	}
	runtime, err := b.resolveSession(session.RuntimeMetadata{
		Profile:  info.Profile,
		Provider: info.Provider,
		Model:    info.Model,
	})
	if err != nil {
		return app.SessionReplacement{}, err
	}
	opener := b.openSession
	if opener == nil {
		opener = openSession
	}
	candidate, warnings, err := opener(path, b.workspacePath)
	if err != nil {
		return app.SessionReplacement{}, b.redactError(err, &runtime)
	}
	runner, err := b.buildRunner(candidate, runtime)
	if err != nil {
		return app.SessionReplacement{}, b.cleanupCandidate(candidate, err, &runtime)
	}
	if runner == nil {
		return app.SessionReplacement{}, b.cleanupCandidate(candidate, errors.New("runner factory returned nil runner"), &runtime)
	}
	return app.SessionReplacement{
		Session: candidate,
		Runner:  runner,
		RuntimeInfo: app.RuntimeInfo{
			Provider: runtime.Provider,
			Profile:  runtime.Profile,
			Model:    runtime.Model,
		},
		Warnings: cloneWarnings(warnings),
	}, nil
}

func (b runtimeBuilder) cleanupCandidate(candidate session.Session, err error, runtime *config.Runtime) error {
	if candidate == nil {
		return b.redactError(err, runtime)
	}
	if closeErr := candidate.Close(); closeErr != nil {
		err = errors.Join(err, closeErr)
	}
	return b.redactError(err, runtime)
}

func (b runtimeBuilder) resumeEnvironment() map[string]string {
	if len(b.environment) == 0 {
		return nil
	}
	environment := make(map[string]string, len(b.environment))
	for key, value := range b.environment {
		switch key {
		case "OTTO_PROVIDER", "OTTO_MODEL", "OTTO_UI":
			continue
		default:
			environment[key] = value
		}
	}
	return environment
}

func (b runtimeBuilder) redactError(err error, runtime *config.Runtime) error {
	if err == nil || errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return err
	}
	message := err.Error()
	for _, value := range b.secretValues(runtime) {
		message = strings.ReplaceAll(message, value, "[REDACTED]")
	}
	if message == err.Error() {
		return err
	}
	return errors.New(message)
}

func (b runtimeBuilder) secretValues(runtime *config.Runtime) []string {
	seen := make(map[string]struct{})
	values := make([]string, 0, len(b.config.Profiles)+2)
	add := func(value string) {
		if value == "" {
			return
		}
		if _, ok := seen[value]; ok {
			return
		}
		seen[value] = struct{}{}
		values = append(values, value)
	}
	add(b.environment["OTTO_API_KEY"])
	for _, profile := range b.config.Profiles {
		if profile.APIKeyEnv != "" {
			add(b.environment[profile.APIKeyEnv])
		}
		collectURLSecretValues(profile.BaseURL, add)
	}
	if runtime != nil {
		add(runtime.APIKey)
		collectURLSecretValues(runtime.BaseURL, add)
	}
	return values
}

func collectURLSecretValues(raw string, add func(string)) {
	if raw == "" {
		return
	}
	parsed, err := url.Parse(raw)
	if err != nil {
		return
	}
	if parsed.User != nil {
		add(parsed.User.Username())
		if password, ok := parsed.User.Password(); ok {
			add(password)
		}
	}
	for _, items := range parsed.Query() {
		for _, item := range items {
			add(item)
		}
	}
	if parsed.Fragment != "" {
		add(parsed.Fragment)
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

func inspectSession(ctx context.Context, path, workspace string) (session.SessionInfo, error) {
	info, _, err := session.Inspect(ctx, path)
	if err != nil {
		return session.SessionInfo{}, err
	}
	if err := validateSessionWorkspace(info.CWD, workspace); err != nil {
		return session.SessionInfo{}, err
	}
	return info, nil
}

func openSession(path, workspace string) (session.Session, []session.Warning, error) {
	store, warnings, err := session.Open(path)
	if err != nil {
		return nil, nil, err
	}
	if err := validateSessionWorkspace(store.Header().Workspace, workspace); err != nil {
		_ = store.Close()
		return nil, nil, err
	}
	return store, cloneWarnings(warnings), nil
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
