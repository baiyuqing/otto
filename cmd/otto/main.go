package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/signal"
	"os/user"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/repl"
	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
	"github.com/baiyuqing/otto/internal/tui"
	"golang.org/x/term"
)

const maxApprovePromptBytes = 1 << 20

var errUnsafeFlagParse = errors.New("unsafe flag parser diagnostic")

func systemPromptFor(definitions []model.ToolDefinition, info app.SandboxInfo) string {
	policy := "Sandbox policy: Bash is unavailable."
	bashUsable := false
	switch {
	case info.Mode == app.SandboxSeatbelt && info.Network == app.SandboxNetworkAllowed && info.BashAvailable && info.Reason == app.SandboxReasonNone:
		policy = "Sandbox policy: Seatbelt confines Bash to workspace-write with network allowed."
		bashUsable = true
	case info.Mode == app.SandboxSeatbelt && info.Network == app.SandboxNetworkDenied && info.BashAvailable && info.Reason == app.SandboxReasonNone:
		policy = "Sandbox policy: Seatbelt confines Bash to workspace-write with network denied."
		bashUsable = true
	case info.Mode == app.SandboxOff && info.Network == app.SandboxNetworkUnconfined && info.BashAvailable && info.Reason == app.SandboxReasonNone:
		policy = "Sandbox policy: Bash is unsandboxed and has the current macOS user's access."
		bashUsable = true
	}

	toolNames := make([]string, 0, len(definitions))
	for _, definition := range definitions {
		if definition.Name == "bash" && !bashUsable {
			continue
		}
		if safePromptToolName(definition.Name) {
			toolNames = append(toolNames, definition.Name)
		}
	}
	tools := "none"
	if len(toolNames) > 0 {
		tools = strings.Join(toolNames, ", ")
	}
	return "You are Otto, a concise coding agent. Inspect the workspace before changing it, including reading AGENTS.md when present and following relevant repository instructions. Usable tools: " + tools + ". File tools are restricted to the workspace. Prefer exact, minimal changes. Report what changed and what verification ran. " + policy
}

func safePromptToolName(name string) bool {
	if name == "" || len(name) > 64 {
		return false
	}
	for index := range len(name) {
		character := name[index]
		if character >= 'a' && character <= 'z' ||
			character >= 'A' && character <= 'Z' ||
			character >= '0' && character <= '9' ||
			character == '_' || character == '-' {
			continue
		}
		return false
	}
	return true
}

type interruptSubscription struct {
	signals <-chan os.Signal
	stop    func()
}

type frontendKind string

const (
	frontendTUI  frontendKind = "tui"
	frontendREPL frontendKind = "repl"
	frontendOnce frontendKind = "once"
)

type terminalDetector func(io.Reader, io.Writer) bool

type environmentEnumerator func() []string

type runDependencies struct {
	subscribeInterrupts  func() interruptSubscription
	prepareSession       func(context.Context, string, string) (preparedSession, error)
	prepareListedSession func(context.Context, string, string, string) (preparedSession, error)
	newSession           func(bool, string, string, config.Runtime) (session.Session, error)
	detectTerminal       terminalDetector
	runTUI               func(context.Context, io.Reader, io.Writer, app.Backend) error
	newRunner            app.RunnerFactory
	openSandbox          func(context.Context, sandboxOpenOptions) sandboxRuntime
	resolveUserHome      func() (string, error)
}

func defaultRunDependencies() runDependencies {
	return runDependencies{
		subscribeInterrupts:  subscribeOSInterrupts,
		prepareSession:       prepareSession,
		prepareListedSession: prepareListedSession,
		newSession:           newSession,
		detectTerminal:       detectTerminalIO,
		runTUI:               tui.Run,
		openSandbox:          openSandboxRuntime,
		resolveUserHome:      currentOSUserHome,
	}
}

func subscribeOSInterrupts() interruptSubscription {
	signals := make(chan os.Signal, 1)
	signal.Notify(signals, os.Interrupt)
	return interruptSubscription{
		signals: signals,
		stop:    func() { signal.Stop(signals) },
	}
}

type cliOptions struct {
	configPath     string
	cwd            string
	profile        string
	provider       string
	baseURL        string
	model          string
	thinking       string
	approve        string
	ui             string
	shellTimeout   time.Duration
	maxOutput      int
	noSession      bool
	continueLast   bool
	resumePath     string
	archivePath    string
	shellTimeSet   bool
	maxOutputSet   bool
	approveSet     bool
	explicitConfig bool
	sandbox        string
	sandboxSet     bool
}

func main() {
	os.Exit(run(context.Background(), os.Args[1:], os.Stdin, os.Stdout, os.Stderr, os.Environ))
}

func run(ctx context.Context, args []string, stdin io.Reader, stdout, stderr io.Writer, enumerate environmentEnumerator) int {
	return runWithDependencies(ctx, args, stdin, stdout, stderr, enumerate, defaultRunDependencies())
}

func runWithDependencies(ctx context.Context, args []string, stdin io.Reader, stdout, stderr io.Writer, enumerate environmentEnumerator, deps runDependencies) int {
	if deps.detectTerminal == nil {
		deps.detectTerminal = detectTerminalIO
	}
	if deps.runTUI == nil {
		deps.runTUI = tui.Run
	}
	if deps.subscribeInterrupts == nil {
		deps.subscribeInterrupts = func() interruptSubscription { return interruptSubscription{stop: func() {}} }
	}
	if deps.openSandbox == nil {
		deps.openSandbox = openSandboxRuntime
	}
	if deps.resolveUserHome == nil {
		deps.resolveUserHome = currentOSUserHome
	}
	if deps.newSession == nil {
		deps.newSession = newSession
	}

	if len(args) > 0 && (args[0] == "memory" || args[0] == "login" || args[0] == "logout") {
		hostEntries, err := captureEnvironment(enumerate)
		if err != nil {
			return fail(stderr, "%v", err)
		}
		environmentLookup, err := newEnvironmentLookup(hostEntries)
		if err != nil {
			return fail(stderr, "%v", err)
		}
		if args[0] == "memory" {
			return runMemoryCommand(ctx, args[1:], stdout, stderr, environmentLookup)
		}
		return runAuthCommand(ctx, args, stdout, stderr, environmentLookup)
	}

	var parseErrors bytes.Buffer
	options, help, err := parseFlags(args, stdout, &parseErrors)
	if help {
		return 0
	}
	if err != nil {
		if errors.Is(err, errUnsafeFlagParse) {
			_, _ = io.WriteString(stderr, "otto: invalid command-line arguments\n")
		} else {
			_, _ = io.WriteString(stderr, parseErrors.String())
		}
		return 2
	}

	hostEntries, err := captureEnvironment(enumerate)
	if err != nil {
		return fail(stderr, "%v", err)
	}
	environmentLookup, err := newEnvironmentLookup(hostEntries)
	if err != nil {
		return fail(stderr, "%v", err)
	}
	processSnapshot, _ := sandbox.ResolveEnvironment(sandbox.EnvironmentOptions{
		HostEntries:   cloneSandboxRuntimeStrings(hostEntries),
		ProviderNames: []string{"OTTO_API_KEY"},
	})
	startupBoundary := runtimeBuilder{
		sandboxSecrets:         processSnapshot.RedactionValues(),
		sandboxSecretsComplete: processSnapshot.RedactionsComplete(),
	}
	startupBoundary.runtimeOverrides.BaseURL = options.baseURL
	redactStartupError := func(err error) error {
		return startupBoundary.redactError(err, nil)
	}

	home, err := resolveHome(environmentLookup, deps.resolveUserHome)
	if err != nil {
		return fail(stderr, "%v", err)
	}
	configPath, configFile, err := loadConfig(options, home)
	if err != nil {
		return fail(stderr, "load config: configuration is invalid or unavailable")
	}
	environment := configEnvironment(configFile, environmentLookup)
	environment["HOME"] = home
	configuredSnapshot, _ := sandbox.ResolveEnvironment(sandbox.EnvironmentOptions{
		HostEntries:   cloneSandboxRuntimeStrings(hostEntries),
		ProviderNames: sandboxProviderEnvironmentNames(configFile, ""),
	})
	startupBoundary.config = configFile
	startupBoundary.environment = environment
	startupBoundary.sandboxSecrets = mergeSandboxRuntimeRedactions(startupBoundary.sandboxSecrets, configuredSnapshot.RedactionValues())
	startupBoundary.sandboxSecretsComplete = startupBoundary.sandboxSecretsComplete && configuredSnapshot.RedactionsComplete()
	if (options.resumePath != "" || options.continueLast) && !startupBoundary.boundaryAllowsDynamic(nil) {
		return fail(stderr, "%v", errSessionOperationUnavailable)
	}

	approvePrompt := options.approve
	if options.approveSet && strings.HasPrefix(approvePrompt, "@") {
		data, err := os.ReadFile(strings.TrimPrefix(approvePrompt, "@"))
		if err != nil {
			return fail(stderr, "%v", redactStartupError(fmt.Errorf("read approve prompt: %w", err)))
		}
		if len(data) > maxApprovePromptBytes {
			return fail(stderr, "read approve prompt: file is too large (%d bytes); maximum is %d bytes", len(data), maxApprovePromptBytes)
		}
		approvePrompt = string(data)
	}

	workspacePath, err := canonicalDirectory(options.cwd)
	if err != nil {
		return fail(stderr, "%v", redactStartupError(fmt.Errorf("resolve cwd: %w", err)))
	}
	workspace, err := tool.NewWorkspace(workspacePath)
	if err != nil {
		return fail(stderr, "%v", redactStartupError(fmt.Errorf("create workspace: %w", err)))
	}
	sessionRoot := filepath.Join(home, ".otto", "sessions")

	if options.archivePath != "" {
		result, err := session.Archive(ctx, sessionRoot, workspacePath, options.archivePath)
		if err != nil {
			return fail(stderr, "archive session: %v", err)
		}
		_, _ = fmt.Fprintf(stdout, "Archived: %s\n", result.Path)
		return 0
	}

	sessionPath := options.resumePath
	listedSessionPath := false
	if options.continueLast {
		listed, listErr := session.List(ctx, sessionRoot, workspacePath, "", 1)
		if listErr != nil {
			return fail(stderr, "%v", redactStartupError(listErr))
		}
		if len(listed.Sessions) == 0 {
			return fail(stderr, "%v", redactStartupError(fmt.Errorf("no session found for workspace %s", workspacePath)))
		}
		sessionPath = listed.Sessions[0].Path
		listedSessionPath = true
	}

	uiMode, err := config.ResolveUIMode(configFile, environment, options.ui)
	if err != nil {
		return fail(stderr, "%v", redactStartupError(err))
	}
	frontend := frontendOnce
	if !options.approveSet {
		frontend, err = selectFrontend(uiMode, stdin, stdout, deps.detectTerminal)
		if err != nil {
			return fail(stderr, "%v", redactStartupError(err))
		}
	}

	shell := environmentLookup.value("SHELL")
	if shell == "" {
		shell = "/bin/sh"
	}
	if canonicalShell, shellErr := canonicalExecutableFile(shell); shellErr == nil {
		shell = canonicalShell
	}
	builder := newRuntimeBuilder(configPath, configFile, environment, workspace, workspacePath, sessionRoot, shell, options, stderr, deps)
	builder.sandboxSecrets = cloneSandboxRuntimeStrings(startupBoundary.sandboxSecrets)
	builder.sandboxSecretsComplete = startupBoundary.sandboxSecretsComplete
	var sandboxDriverOverride *string
	if options.sandboxSet {
		driver := strings.Clone(options.sandbox)
		sandboxDriverOverride = &driver
	}
	sandboxSettings, err := config.ResolveSandbox(configFile.Sandbox, sandboxDriverOverride)
	if err != nil {
		return fail(stderr, "%v", builder.redactError(err, nil))
	}

	memoryCfg, err := config.ResolveMemory(configFile, environment, config.Overrides{})
	if err != nil {
		return fail(stderr, "%v", err)
	}

	var (
		metadata        *session.RuntimeMetadata
		preparedInitial preparedSession
		preparedInfo    session.SessionInfo
	)
	if sessionPath != "" {
		if listedSessionPath {
			preparedInitial, err = builder.prepareListed(ctx, sessionPath)
		} else {
			preparedInitial, err = builder.prepare(ctx, sessionPath)
		}
		if err != nil {
			return fail(stderr, "%v", builder.redactError(err, nil))
		}
		defer preparedInitial.Close()
		preparedInfo = preparedInitial.Info()
		metadata = &session.RuntimeMetadata{Profile: preparedInfo.Profile, Provider: preparedInfo.Provider, Model: preparedInfo.Model}
	}
	overrides := config.Overrides{
		Profile:        options.profile,
		Provider:       options.provider,
		BaseURL:        options.baseURL,
		Model:          options.model,
		Thinking:       options.thinking,
		ShellTimeout:   options.shellTimeout,
		MaxOutputBytes: options.maxOutput,
	}
	resolvedRuntime, err := resolveInitialRuntime(configFile, environment, metadata, overrides)
	if err != nil {
		return fail(stderr, "%v", builder.redactError(err, nil))
	}

	processCtx, cancelProcess := context.WithCancel(ctx)
	defer cancelProcess()
	subscription := deps.subscribeInterrupts()
	if subscription.stop == nil {
		subscription.stop = func() {}
	}
	var replMu sync.Mutex
	var currentREPL *repl.REPL
	signalDone := make(chan struct{})
	signalStopped := make(chan struct{})
	go func() {
		defer close(signalStopped)
		for {
			select {
			case <-signalDone:
				return
			case <-subscription.signals:
				replMu.Lock()
				active := currentREPL != nil && currentREPL.Interrupt()
				replMu.Unlock()
				if !active {
					cancelProcess()
				}
			}
		}
	}()
	defer func() {
		subscription.stop()
		close(signalDone)
		<-signalStopped
	}()
	processSandbox := deps.openSandbox(processCtx, sandboxOpenOptions{
		Settings:      sandboxSettings,
		Workspace:     workspacePath,
		Shell:         shell,
		Home:          home,
		HostEntries:   hostEntries,
		ProviderNames: sandboxProviderEnvironmentNames(configFile, resolvedRuntime.APIKeyEnv),
	})
	sandboxClosed := false
	closeSandbox := func() error {
		if sandboxClosed {
			return nil
		}
		sandboxClosed = true
		return processSandbox.Close()
	}
	var controller *app.Controller
	controllerClosed := false
	defer func() {
		cancelProcess()
		if controller != nil && !controllerClosed {
			controllerClosed = true
			_ = controller.Close()
		}
		if !sandboxClosed {
			_ = closeSandbox()
		}
	}()
	if processCtx.Err() != nil {
		cancelProcess()
		_ = closeSandbox()
		return 130
	}
	if !processSandbox.RedactionsComplete {
		processSandbox.Executor = nil
		processSandbox.Environment = nil
		processSandbox.Info = app.SandboxInfo{
			Mode: app.SandboxUnavailable, BashAvailable: false, Reason: app.SandboxReasonEnvironmentRejected,
		}
	} else if processSandbox.Info.BashAvailable && (isNilSandboxRuntimeValue(processSandbox.Executor) || processSandbox.Environment == nil) {
		processSandbox.Executor = nil
		processSandbox.Environment = nil
		processSandbox.Info = app.SandboxInfo{
			Mode: app.SandboxUnavailable, BashAvailable: false, Reason: app.SandboxReasonRuntimeFailure,
		}
	}

	builder.commandExecutor = processSandbox.Executor
	builder.sandboxEnvironment = cloneSandboxRuntimeStrings(processSandbox.Environment)
	builder.sandboxInfo = processSandbox.Info
	builder.sandboxSecrets = mergeSandboxRuntimeRedactions(builder.sandboxSecrets, processSandbox.RedactionValues)
	builder.sandboxSecretsComplete = builder.sandboxSecretsComplete && processSandbox.RedactionsComplete
	printSandboxRuntimeWarning(stderr, builder.effectiveSandboxInfo())
	if processCtx.Err() != nil {
		return 130
	}
	dynamicContent := builder.boundaryAllowsDynamic(&resolvedRuntime)
	if preparedInitial != nil && !dynamicContent {
		return fail(stderr, "%v", errSessionOperationUnavailable)
	}
	if processCtx.Err() != nil {
		return 130
	}

	memoryService, memoryUserScope, memoryUsable, err := openMemoryService(processCtx, memoryCfg, collectSecretValues(configFile, environment, &resolvedRuntime), stderr)
	if err != nil {
		return fail(stderr, "%v", builder.redactError(err, &resolvedRuntime))
	}
	defer func() { _ = memoryService.Close() }()
	memoryWorkspaceScope, err := workspaceMemoryScope(memoryCfg, workspacePath)
	if err != nil {
		return fail(stderr, "%v", err)
	}
	builder.memoryService = memoryService
	builder.memoryUsable = memoryUsable
	builder.memoryUserScope = memoryUserScope
	builder.memoryWorkspaceScope = memoryWorkspaceScope
	builder.memoryRecallLimit = memoryCfg.MaxResults
	builder.memoryRecallTokenBudget = memoryCfg.RecallTokens

	var (
		initialSession  session.Session
		startupWarnings []session.Warning
	)
	if preparedInitial != nil {
		initialSession, startupWarnings, err = builder.activatePrepared(processCtx, preparedInitial, preparedInfo, &resolvedRuntime)
	} else if !dynamicContent {
		initialSession = session.NewMemory(session.Header{Version: session.CurrentVersion})
	} else {
		initialSession, err = deps.newSession(options.noSession, sessionRoot, workspacePath, resolvedRuntime)
	}
	if processCtx.Err() != nil {
		if initialSession != nil {
			_ = initialSession.Close()
		}
		return 130
	}
	if err != nil {
		return fail(stderr, "%v", builder.redactError(err, &resolvedRuntime))
	}
	printWarnings(stderr, startupWarnings)
	if processCtx.Err() != nil {
		_ = initialSession.Close()
		return 130
	}

	initialRunner, err := builder.buildRunner(processCtx, initialSession, resolvedRuntime)
	if processCtx.Err() != nil {
		_ = initialSession.Close()
		return 130
	}
	if err != nil {
		_ = initialSession.Close()
		return fail(stderr, "%v", builder.redactError(err, &resolvedRuntime))
	}
	if err := builder.updateSessionRuntime(processCtx, initialSession, resolvedRuntime); err != nil {
		_ = initialSession.Close()
		if processCtx.Err() != nil {
			return 130
		}
		return fail(stderr, "%v", builder.redactError(err, &resolvedRuntime))
	}
	if processCtx.Err() != nil {
		_ = initialSession.Close()
		return 130
	}
	initialRunnerPending := true
	buildRunner := func(current session.Session) app.Runner {
		if initialRunnerPending {
			initialRunnerPending = false
			return initialRunner
		}
		runner, buildErr := builder.buildRunner(context.Background(), current, resolvedRuntime)
		if buildErr != nil {
			return nil
		}
		return runner
	}
	controllerOptions := []app.Option{
		app.WithRuntimeInfo(builder.runtimeInfo(resolvedRuntime)),
		app.WithDynamicContent(dynamicContent),
		app.WithMemory(memoryService, memoryUserScope, memoryWorkspaceScope),
		app.WithProfileSwitcher(builder.profileNames(), builder.buildProfileReplacement),
		app.WithDefaultProfileSetter(builder.persistDefaultProfile),
		app.WithNewSessionBuilder(builder.buildNewReplacement),
	}
	if !options.noSession {
		controllerOptions = append(controllerOptions,
			app.WithSessionBrowser(func(ctx context.Context, limit int) (session.ListResult, error) {
				return session.List(ctx, sessionRoot, workspacePath, "", limit)
			}, builder.openReplacement),
			app.WithSessionArchiver(func(ctx context.Context, path string) (session.ArchiveResult, error) {
				return session.Archive(ctx, sessionRoot, workspacePath, path)
			}),
		)
	}
	createSession := func() (session.Session, error) {
		if !dynamicContent {
			return nil, errSessionOperationUnavailable
		}
		return deps.newSession(options.noSession, sessionRoot, workspacePath, resolvedRuntime)
	}
	controller, err = app.New(initialSession, createSession, buildRunner, controllerOptions...)
	if err != nil {
		_ = initialSession.Close()
		if processCtx.Err() != nil {
			return 130
		}
		return fail(stderr, "%v", builder.redactError(err, &resolvedRuntime))
	}
	if processCtx.Err() != nil {
		cancelProcess()
		controllerClosed = true
		_ = controller.Close()
		return 130
	}
	closeController := func() error {
		if controllerClosed {
			return nil
		}
		controllerClosed = true
		return controller.Close()
	}

	var runErr error
	switch frontend {
	case frontendTUI:
		runErr = deps.runTUI(processCtx, stdin, stdout, controller)
	case frontendOnce:
		console := repl.New(strings.NewReader(""), stdout, stderr, controller)
		replMu.Lock()
		currentREPL = console
		replMu.Unlock()
		runErr = console.RunOnce(processCtx, approvePrompt)
		replMu.Lock()
		if currentREPL == console {
			currentREPL = nil
		}
		replMu.Unlock()
	case frontendREPL:
		input := repl.NewInput(stdin)
		console := repl.NewWithInput(input, stdout, stderr, controller)

		replMu.Lock()
		currentREPL = console
		replMu.Unlock()
		runErr = console.Run(processCtx)
		replMu.Lock()
		if currentREPL == console {
			currentREPL = nil
		}
		replMu.Unlock()
	default:
		return fail(stderr, "unsupported frontend %q", frontend)
	}

	processCanceledBeforeFrontendExit := processCtx.Err() != nil
	frontendCanceled := errors.Is(runErr, context.Canceled)
	cancelProcess()
	controllerErr := closeController()
	sandboxCloseErr := closeSandbox()
	if controllerErr != nil {
		return fail(stderr, "close session: %v", builder.redactError(controllerErr, &resolvedRuntime))
	}
	if sandboxCloseErr != nil {
		return fail(stderr, "close sandbox: %v", errSandboxRuntimeClose)
	}
	if frontend == frontendTUI && errors.Is(runErr, session.ErrFatalPersistence) {
		return fail(stderr, "TUI: %v", builder.redactError(runErr, &resolvedRuntime))
	}
	if processCanceledBeforeFrontendExit || frontendCanceled {
		return 130
	}
	if frontend == frontendOnce && runErr != nil {
		// RunOnce already rendered the error to stderr.
		return 1
	}
	if frontend == frontendREPL && repl.IsCommandError(runErr, "/new") {
		return fail(stderr, "%v", builder.redactError(runErr, &resolvedRuntime))
	}
	if runErr != nil {
		if frontend == frontendTUI {
			return fail(stderr, "TUI: %v", builder.redactError(runErr, &resolvedRuntime))
		}
		return fail(stderr, "REPL: %v", builder.redactError(runErr, &resolvedRuntime))
	}
	return 0
}

func parseFlags(args []string, stdout, stderr io.Writer) (cliOptions, bool, error) {
	options := cliOptions{cwd: "."}
	flags := flag.NewFlagSet("otto", flag.ContinueOnError)
	flags.SetOutput(io.Discard)
	var showHelp bool
	flags.BoolVar(&showHelp, "help", false, "show help")
	flags.BoolVar(&showHelp, "h", false, "show help")
	flags.StringVar(&options.configPath, "config", "", "configuration file")
	flags.StringVar(&options.cwd, "cwd", ".", "workspace directory")
	flags.StringVar(&options.profile, "profile", "", "configuration profile")
	flags.StringVar(&options.provider, "provider", "", "provider override")
	flags.StringVar(&options.baseURL, "base-url", "", "provider base URL override")
	flags.StringVar(&options.model, "model", "", "model override")
	flags.StringVar(&options.thinking, "thinking", "", "model thinking effort: low, medium, high, xhigh, or max")
	flags.StringVar(&options.approve, "approve", "", "run PROMPT (or @FILE) without interaction and exit")
	flags.StringVar(&options.ui, "ui", "", "frontend mode: auto, tui, or repl")
	flags.StringVar(&options.sandbox, "sandbox", "", "sandbox mode: auto, seatbelt, or off (off is unsafe)")
	flags.DurationVar(&options.shellTimeout, "shell-timeout", 0, "shell command timeout")
	flags.IntVar(&options.maxOutput, "max-output-bytes", 0, "maximum tool output bytes")
	flags.BoolVar(&options.noSession, "no-session", false, "use an in-memory session")
	flags.BoolVar(&options.continueLast, "continue", false, "continue the newest workspace session")
	flags.StringVar(&options.resumePath, "resume", "", "resume a session file")
	flags.StringVar(&options.archivePath, "archive", "", "archive an active session file")
	flags.Usage = func() {}
	if err := flags.Parse(args); err != nil {
		return options, false, errUnsafeFlagParse
	}
	if showHelp {
		printUsage(stdout)
		return options, true, nil
	}
	if flags.NArg() != 0 {
		_, _ = fmt.Fprintln(stderr, "otto: unexpected positional arguments")
		return options, false, errors.New("unexpected positional arguments")
	}
	visited := make(map[string]bool)
	flags.Visit(func(item *flag.Flag) { visited[item.Name] = true })
	options.explicitConfig = visited["config"]
	options.shellTimeSet = visited["shell-timeout"]
	options.maxOutputSet = visited["max-output-bytes"]
	options.approveSet = visited["approve"]
	options.sandboxSet = visited["sandbox"]
	if options.sandboxSet {
		switch options.sandbox {
		case string(sandbox.DriverAuto), string(sandbox.DriverSeatbelt), string(sandbox.DriverOff):
		default:
			_, _ = fmt.Fprintln(stderr, "otto: --sandbox must be one of auto, seatbelt, off")
			return options, false, errors.New("invalid sandbox mode")
		}
	}
	if options.continueLast && options.resumePath != "" {
		_, _ = fmt.Fprintln(stderr, "otto: --continue and --resume cannot be used together")
		return options, false, errors.New("conflicting session flags")
	}
	if options.noSession && (options.continueLast || options.resumePath != "") {
		_, _ = fmt.Fprintln(stderr, "otto: --no-session cannot be used with --continue or --resume")
		return options, false, errors.New("conflicting session flags")
	}
	if options.archivePath != "" {
		conflict := ""
		switch {
		case options.continueLast:
			conflict = "--continue"
		case options.resumePath != "":
			conflict = "--resume"
		case options.noSession:
			conflict = "--no-session"
		case options.approveSet:
			conflict = "--approve"
		}
		if conflict != "" {
			_, _ = fmt.Fprintf(stderr, "otto: --archive cannot be used with %s\n", conflict)
			return options, false, errors.New("conflicting archive flag")
		}
	}
	if options.shellTimeSet && options.shellTimeout <= 0 {
		_, _ = fmt.Fprintln(stderr, "otto: --shell-timeout must be greater than zero")
		return options, false, errors.New("invalid shell timeout")
	}
	if options.maxOutputSet && options.maxOutput <= 0 {
		_, _ = fmt.Fprintln(stderr, "otto: --max-output-bytes must be greater than zero")
		return options, false, errors.New("invalid max output")
	}
	switch options.thinking {
	case "", "low", "medium", "high", "xhigh", "max":
	default:
		_, _ = fmt.Fprintln(stderr, "otto: --thinking must be one of low, medium, high, xhigh, max")
		return options, false, errors.New("invalid thinking level")
	}
	if options.approveSet && strings.TrimSpace(options.approve) == "" {
		_, _ = fmt.Fprintln(stderr, "otto: --approve requires a non-empty prompt")
		return options, false, errors.New("empty approve prompt")
	}
	if options.approveSet && options.ui == "tui" {
		_, _ = fmt.Fprintln(stderr, "otto: --approve cannot be used with --ui tui")
		return options, false, errors.New("conflicting frontend flags")
	}
	return options, false, nil
}

func printUsage(output io.Writer) {
	_, _ = io.WriteString(output, `Usage: otto [options]
       otto login [--status]   sign in with a ChatGPT subscription
       otto logout             remove stored ChatGPT credentials
       otto memory status|forget <id>

Sandbox: on macOS, auto -> Seatbelt; if it cannot be established, bash is disabled.
WARNING: off is explicitly unsafe; bash runs unsandboxed with anything accessible to your macOS user.
File tools always stay within the selected workspace.

Options:
  --help                 show help
  --config PATH          configuration file
  --cwd PATH             workspace directory
  --profile NAME         configuration profile
  --provider NAME        provider override
  --base-url URL         provider base URL override
  --model NAME           model override
  --thinking LEVEL       model thinking effort: low, medium, high, xhigh, or max
  --approve PROMPT       run PROMPT (or @FILE) without interaction and exit
  --ui MODE              frontend mode: auto, tui, or repl
  --sandbox MODE         sandbox mode: auto, seatbelt, or off (off is unsafe)
  --shell-timeout D      shell command timeout
  --max-output-bytes N   maximum tool output bytes
  --no-session           use an in-memory session
  --continue             continue newest workspace session
  --resume PATH          resume a session file
  --archive PATH         archive an active session file
`)
}

func canonicalDirectory(path string) (string, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	canonical, err := filepath.EvalSymlinks(absolute)
	if err != nil {
		return "", err
	}
	info, err := os.Stat(canonical)
	if err != nil {
		return "", err
	}
	if !info.IsDir() {
		return "", fmt.Errorf("not a directory: %s", canonical)
	}
	return canonical, nil
}

const (
	// Darwin's process argument/environment budget is about 1 MiB. These
	// ceilings are deliberately larger while still bounding injected snapshots.
	maxLookupEnvironmentNameBytes  = 4 << 10
	maxLookupEnvironmentEntryBytes = 1 << 20
	maxLookupEnvironmentEntries    = 1 << 18
	maxLookupEnvironmentBytes      = 8 << 20
	maxCapturedEnvironmentEntries  = 1 << 19
	maxCapturedEnvironmentBytes    = 16 << 20
)

var errEnvironmentSnapshotTooLarge = errors.New("process environment snapshot is too large")

type environmentLookup map[string]string

func captureEnvironment(enumerate environmentEnumerator) ([]string, error) {
	if enumerate == nil {
		return []string{}, nil
	}
	entries := enumerate()
	if len(entries) > maxCapturedEnvironmentEntries {
		return nil, errEnvironmentSnapshotTooLarge
	}
	total := 0
	for _, entry := range entries {
		if len(entry) > maxCapturedEnvironmentBytes-total {
			return nil, errEnvironmentSnapshotTooLarge
		}
		total += len(entry)
	}
	captured := make([]string, len(entries))
	for index, entry := range entries {
		captured[index] = strings.Clone(entry)
	}
	return captured, nil
}

func newEnvironmentLookup(entries []string) (environmentLookup, error) {
	return newEnvironmentLookupWithLimits(entries, maxLookupEnvironmentEntries, maxLookupEnvironmentBytes)
}

func newEnvironmentLookupWithLimits(entries []string, maxEntries, maxBytes int) (environmentLookup, error) {
	if maxEntries < 0 || maxBytes < 0 {
		return nil, errEnvironmentSnapshotTooLarge
	}
	parsed := make(map[string]string)
	total := 0
	for _, entry := range entries {
		if len(entry) > maxLookupEnvironmentEntryBytes || !utf8.ValidString(entry) || strings.IndexByte(entry, 0) >= 0 {
			continue
		}
		name, value, found := strings.Cut(entry, "=")
		if !found || len(name) > maxLookupEnvironmentNameBytes || !validLookupEnvironmentName(name) {
			continue
		}
		previous, duplicate := parsed[name]
		if !duplicate && len(parsed) >= maxEntries {
			return nil, errEnvironmentSnapshotTooLarge
		}
		nextTotal := total + len(name) + len(value)
		if duplicate {
			nextTotal -= len(name) + len(previous)
		}
		if nextTotal > maxBytes {
			return nil, errEnvironmentSnapshotTooLarge
		}
		parsed[name] = value
		total = nextTotal
	}

	lookup := make(environmentLookup, len(parsed))
	for name, value := range parsed {
		lookup[strings.Clone(name)] = strings.Clone(value)
	}
	return lookup, nil
}

func (lookup environmentLookup) value(name string) string {
	return lookup[name]
}

func validLookupEnvironmentName(name string) bool {
	if name == "" || !(name[0] == '_' || name[0] >= 'A' && name[0] <= 'Z' || name[0] >= 'a' && name[0] <= 'z') {
		return false
	}
	for index := 1; index < len(name); index++ {
		character := name[index]
		if character != '_' && (character < 'A' || character > 'Z') && (character < 'a' || character > 'z') && (character < '0' || character > '9') {
			return false
		}
	}
	return true
}

func currentOSUserHome() (string, error) {
	current, err := user.Current()
	if err != nil || current == nil || current.HomeDir == "" {
		return "", errors.New("resolve home directory")
	}
	return strings.Clone(current.HomeDir), nil
}

func resolveHome(lookup environmentLookup, fallback func() (string, error)) (string, error) {
	home := lookup.value("HOME")
	if home == "" {
		if fallback == nil {
			return "", errors.New("resolve home directory")
		}
		var err error
		home, err = fallback()
		if err != nil || home == "" {
			return "", errors.New("resolve home directory")
		}
	}
	absolute, err := filepath.Abs(home)
	if err != nil {
		return "", errors.New("resolve home directory")
	}
	return absolute, nil
}

func loadConfig(options cliOptions, home string) (string, config.File, error) {
	path := options.configPath
	if path == "" {
		path = filepath.Join(home, ".config", "otto", "config.toml")
	}
	file, err := config.LoadRequired(path)
	if err != nil && !options.explicitConfig && os.IsNotExist(err) {
		return path, config.File{}, nil
	}
	return path, file, err
}

func configEnvironment(file config.File, lookup environmentLookup) map[string]string {
	keys := map[string]struct{}{
		"HOME":          {},
		"OTTO_PROVIDER": {},
		"OTTO_PROFILE":  {},
		"OTTO_MODEL":    {},
		"OTTO_API_KEY":  {},
		"OTTO_UI":       {},
	}
	for _, profile := range file.Profiles {
		if profile.APIKeyEnv != "" {
			keys[profile.APIKeyEnv] = struct{}{}
		}
	}
	environment := make(map[string]string, len(keys))
	for key := range keys {
		environment[key] = lookup.value(key)
	}
	return environment
}

func sandboxProviderEnvironmentNames(file config.File, selected string) []string {
	seen := make(map[string]struct{}, len(file.Profiles)+2)
	names := make([]string, 0, len(file.Profiles)+2)
	add := func(name string) {
		if name == "" {
			return
		}
		if _, exists := seen[name]; exists {
			return
		}
		seen[name] = struct{}{}
		names = append(names, strings.Clone(name))
	}
	add("OTTO_API_KEY")
	add(selected)
	for _, profile := range file.Profiles {
		add(profile.APIKeyEnv)
	}
	sort.Strings(names)
	return names
}

func printSandboxRuntimeWarning(output io.Writer, info app.SandboxInfo) {
	switch info.Mode {
	case app.SandboxUnavailable:
		reason := safeSandboxReason(app.SandboxReason(info.ReasonCode()))
		_, _ = fmt.Fprintf(output, "warning: bash is unavailable because the configured sandbox could not be established (reason: %s); file tools remain available\n", reason)
	case app.SandboxOff:
		if info.Network == app.SandboxNetworkUnconfined && info.BashAvailable && info.Reason == app.SandboxReasonNone {
			_, _ = fmt.Fprintln(output, "warning: sandbox is off; bash runs unsandboxed as your macOS user")
		}
	}
}

func detectTerminalIO(input io.Reader, output io.Writer) bool {
	inputFile, ok := input.(*os.File)
	if !ok {
		return false
	}
	outputFile, ok := output.(*os.File)
	if !ok {
		return false
	}
	return term.IsTerminal(int(inputFile.Fd())) && term.IsTerminal(int(outputFile.Fd()))
}

func selectFrontend(mode config.UIMode, input io.Reader, output io.Writer, detect terminalDetector) (frontendKind, error) {
	if detect == nil {
		detect = func(io.Reader, io.Writer) bool { return false }
	}

	switch mode {
	case config.UIAuto:
		if detect(input, output) {
			return frontendTUI, nil
		}
		return frontendREPL, nil
	case config.UITUI:
		if detect(input, output) {
			return frontendTUI, nil
		}
		return "", errors.New("--ui tui requires terminal stdin and stdout; use --ui repl for redirected input")
	case config.UIRepl:
		return frontendREPL, nil
	default:
		return "", fmt.Errorf("unsupported ui mode %q", mode)
	}
}

func newSession(memory bool, root, workspace string, runtime config.Runtime) (session.Session, error) {
	id, err := randomID()
	if err != nil {
		return nil, fmt.Errorf("create session id: %w", err)
	}
	header := session.Header{
		Version: session.CurrentVersion, ID: id, Workspace: workspace, Provider: runtime.Provider,
		Profile: runtime.Profile, Model: runtime.Model, CreatedAt: time.Now().UTC(),
	}
	if memory {
		return session.NewMemory(header), nil
	}
	// CreateLazy defers file creation until the first user message, so
	// starting Otto and quitting without any prompt leaves no session file.
	store, err := session.CreateLazy(root, header)
	if err != nil {
		return nil, err
	}
	return store, nil
}

func randomID() (string, error) {
	data := make([]byte, 16)
	if _, err := rand.Read(data); err != nil {
		return "", err
	}
	return hex.EncodeToString(data), nil
}

func fail(stderr io.Writer, format string, arguments ...any) int {
	_, _ = fmt.Fprintf(stderr, "otto: "+format+"\n", arguments...)
	return 1
}
