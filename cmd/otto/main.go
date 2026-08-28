package main

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"sync"
	"time"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/repl"
	"github.com/baiyuqing/otto/internal/session"
	"github.com/baiyuqing/otto/internal/tool"
	"github.com/baiyuqing/otto/internal/tui"
	"golang.org/x/term"
)

const systemPrompt = "You are Otto, a concise coding agent. Inspect the workspace before changing it. Use read, write, edit, and bash when needed. File tools are restricted to the workspace, but bash is unsandboxed. Prefer exact, minimal changes. Report what changed and what verification ran."

type interruptSubscription struct {
	signals <-chan os.Signal
	stop    func()
}

type frontendKind string

const (
	frontendTUI  frontendKind = "tui"
	frontendREPL frontendKind = "repl"
)

type terminalDetector func(io.Reader, io.Writer) bool

type runDependencies struct {
	subscribeInterrupts  func() interruptSubscription
	prepareSession       func(context.Context, string, string) (preparedSession, error)
	prepareListedSession func(context.Context, string, string, string) (preparedSession, error)
	newSession           func(bool, string, string, config.Runtime) (session.Session, error)
	detectTerminal       terminalDetector
	runTUI               func(context.Context, io.Reader, io.Writer, app.Backend) error
	newRunner            app.RunnerFactory
}

func defaultRunDependencies() runDependencies {
	return runDependencies{
		subscribeInterrupts:  subscribeOSInterrupts,
		prepareSession:       prepareSession,
		prepareListedSession: prepareListedSession,
		newSession:           newSession,
		detectTerminal:       detectTerminalIO,
		runTUI:               tui.Run,
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
	ui             string
	shellTimeout   time.Duration
	maxOutput      int
	noSession      bool
	continueLast   bool
	resumePath     string
	shellTimeSet   bool
	maxOutputSet   bool
	explicitConfig bool
}

func main() {
	os.Exit(run(context.Background(), os.Args[1:], os.Stdin, os.Stdout, os.Stderr, os.Getenv))
}

func run(ctx context.Context, args []string, stdin io.Reader, stdout, stderr io.Writer, getenv func(string) string) int {
	return runWithDependencies(ctx, args, stdin, stdout, stderr, getenv, defaultRunDependencies())
}

func runWithDependencies(ctx context.Context, args []string, stdin io.Reader, stdout, stderr io.Writer, getenv func(string) string, deps runDependencies) int {
	if deps.detectTerminal == nil {
		deps.detectTerminal = detectTerminalIO
	}
	if deps.runTUI == nil {
		deps.runTUI = tui.Run
	}
	if deps.subscribeInterrupts == nil {
		deps.subscribeInterrupts = func() interruptSubscription { return interruptSubscription{stop: func() {}} }
	}

	options, help, err := parseFlags(args, stdout, stderr)
	if help {
		return 0
	}
	if err != nil {
		return 2
	}

	workspacePath, err := canonicalDirectory(options.cwd)
	if err != nil {
		return fail(stderr, "resolve cwd: %v", err)
	}
	workspace, err := tool.NewWorkspace(workspacePath)
	if err != nil {
		return fail(stderr, "create workspace: %v", err)
	}

	home, err := resolveHome(getenv)
	if err != nil {
		return fail(stderr, "%v", err)
	}
	sessionRoot := filepath.Join(home, ".otto", "sessions")

	sessionPath := options.resumePath
	listedSessionPath := false
	if options.continueLast {
		listed, listErr := session.List(ctx, sessionRoot, workspacePath, "", 1)
		if listErr != nil {
			return fail(stderr, "%v", listErr)
		}
		if len(listed.Sessions) == 0 {
			return fail(stderr, "no session found for workspace %s", workspacePath)
		}
		sessionPath = listed.Sessions[0].Path
		listedSessionPath = true
	}

	configFile, err := loadConfig(options, home)
	if err != nil {
		return fail(stderr, "load config: %v", err)
	}
	environment := configEnvironment(configFile, getenv)
	uiMode, err := config.ResolveUIMode(configFile, environment, options.ui)
	if err != nil {
		return fail(stderr, "%v", err)
	}
	frontend, err := selectFrontend(uiMode, stdin, stdout, deps.detectTerminal)
	if err != nil {
		return fail(stderr, "%v", err)
	}

	shell := getenv("SHELL")
	if shell == "" {
		shell = "/bin/sh"
	}
	builder := newRuntimeBuilder(configFile, environment, workspace, workspacePath, sessionRoot, shell, options, stderr, deps)

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
		ShellTimeout:   options.shellTimeout,
		MaxOutputBytes: options.maxOutput,
	}
	runtime, err := resolveInitialRuntime(configFile, environment, metadata, overrides)
	if err != nil {
		return fail(stderr, "%v", builder.redactError(err, nil))
	}
	if err := validateShell(shell); err != nil {
		return fail(stderr, "%v", err)
	}

	var (
		initialSession  session.Session
		startupWarnings []session.Warning
	)
	if preparedInitial != nil {
		initialSession, startupWarnings, err = builder.activatePrepared(ctx, preparedInitial, preparedInfo, &runtime)
	} else {
		initialSession, err = deps.newSession(options.noSession, sessionRoot, workspacePath, runtime)
	}
	if err != nil {
		return fail(stderr, "%v", err)
	}
	printWarnings(stderr, startupWarnings)

	initialRunner, err := builder.buildRunner(initialSession, runtime)
	if err != nil {
		_ = initialSession.Close()
		return fail(stderr, "%v", builder.redactError(err, &runtime))
	}
	if err := updateSessionRuntime(ctx, initialSession, runtime); err != nil {
		_ = initialSession.Close()
		return fail(stderr, "%v", builder.redactError(err, &runtime))
	}
	initialRunnerPending := true
	buildRunner := func(current session.Session) app.Runner {
		if initialRunnerPending {
			initialRunnerPending = false
			return initialRunner
		}
		runner, buildErr := builder.buildRunner(current, runtime)
		if buildErr != nil {
			return nil
		}
		return runner
	}
	controllerOptions := []app.Option{app.WithRuntimeInfo(app.RuntimeInfo{
		Provider: runtime.Provider,
		Profile:  runtime.Profile,
		Model:    runtime.Model,
	})}
	if !options.noSession {
		controllerOptions = append(controllerOptions, app.WithSessionBrowser(func(ctx context.Context, limit int) (session.ListResult, error) {
			return session.List(ctx, sessionRoot, workspacePath, "", limit)
		}, builder.openReplacement))
	}
	controller, err := app.New(initialSession, func() (session.Session, error) {
		return deps.newSession(options.noSession, sessionRoot, workspacePath, runtime)
	}, buildRunner, controllerOptions...)
	if err != nil {
		_ = initialSession.Close()
		return fail(stderr, "%v", err)
	}
	controllerClosed := false
	closeController := func() error {
		if controllerClosed {
			return nil
		}
		controllerClosed = true
		return controller.Close()
	}
	defer func() {
		if !controllerClosed {
			_ = controller.Close()
		}
	}()

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

	var runErr error
	switch frontend {
	case frontendTUI:
		runErr = deps.runTUI(processCtx, stdin, stdout, controller)
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
	if err := closeController(); err != nil {
		return fail(stderr, "close session: %v", err)
	}
	if frontend == frontendTUI && errors.Is(runErr, session.ErrFatalPersistence) {
		return fail(stderr, "TUI: %v", runErr)
	}
	if processCanceledBeforeFrontendExit || frontendCanceled {
		return 130
	}
	if frontend == frontendREPL && repl.IsCommandError(runErr, "/new") {
		return fail(stderr, "%v", runErr)
	}
	if runErr != nil {
		if frontend == frontendTUI {
			return fail(stderr, "TUI: %v", runErr)
		}
		return fail(stderr, "REPL: %v", runErr)
	}
	return 0
}

func parseFlags(args []string, stdout, stderr io.Writer) (cliOptions, bool, error) {
	options := cliOptions{cwd: "."}
	flags := flag.NewFlagSet("otto", flag.ContinueOnError)
	flags.SetOutput(stderr)
	var showHelp bool
	flags.BoolVar(&showHelp, "help", false, "show help")
	flags.BoolVar(&showHelp, "h", false, "show help")
	flags.StringVar(&options.configPath, "config", "", "configuration file")
	flags.StringVar(&options.cwd, "cwd", ".", "workspace directory")
	flags.StringVar(&options.profile, "profile", "", "configuration profile")
	flags.StringVar(&options.provider, "provider", "", "provider override")
	flags.StringVar(&options.baseURL, "base-url", "", "provider base URL override")
	flags.StringVar(&options.model, "model", "", "model override")
	flags.StringVar(&options.ui, "ui", "", "frontend mode: auto, tui, or repl")
	flags.DurationVar(&options.shellTimeout, "shell-timeout", 0, "shell command timeout")
	flags.IntVar(&options.maxOutput, "max-output-bytes", 0, "maximum tool output bytes")
	flags.BoolVar(&options.noSession, "no-session", false, "use an in-memory session")
	flags.BoolVar(&options.continueLast, "continue", false, "continue the newest workspace session")
	flags.StringVar(&options.resumePath, "resume", "", "resume a session file")
	flags.Usage = func() { printUsage(stderr) }
	if err := flags.Parse(args); err != nil {
		return options, false, err
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
	if options.continueLast && options.resumePath != "" {
		_, _ = fmt.Fprintln(stderr, "otto: --continue and --resume cannot be used together")
		return options, false, errors.New("conflicting session flags")
	}
	if options.noSession && (options.continueLast || options.resumePath != "") {
		_, _ = fmt.Fprintln(stderr, "otto: --no-session cannot be used with --continue or --resume")
		return options, false, errors.New("conflicting session flags")
	}
	if options.shellTimeSet && options.shellTimeout <= 0 {
		_, _ = fmt.Fprintln(stderr, "otto: --shell-timeout must be greater than zero")
		return options, false, errors.New("invalid shell timeout")
	}
	if options.maxOutputSet && options.maxOutput <= 0 {
		_, _ = fmt.Fprintln(stderr, "otto: --max-output-bytes must be greater than zero")
		return options, false, errors.New("invalid max output")
	}
	return options, false, nil
}

func printUsage(output io.Writer) {
	_, _ = io.WriteString(output, `Usage: otto [options]

WARNING: bash is unsandboxed and can access anything accessible to your macOS user.
File tools stay within the selected workspace; shell commands do not.

Options:
  --help                 show help
  --config PATH          configuration file
  --cwd PATH             workspace directory
  --profile NAME         configuration profile
  --provider NAME        provider override
  --base-url URL         provider base URL override
  --model NAME           model override
  --ui MODE              frontend mode: auto, tui, or repl
  --shell-timeout D      shell command timeout
  --max-output-bytes N   maximum tool output bytes
  --no-session           use an in-memory session
  --continue             continue newest workspace session
  --resume PATH          resume a session file
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

func resolveHome(getenv func(string) string) (string, error) {
	if home := getenv("HOME"); home != "" {
		return filepath.Abs(home)
	}
	home, err := os.UserHomeDir()
	if err != nil || home == "" {
		return "", errors.New("resolve home directory")
	}
	return filepath.Abs(home)
}

func loadConfig(options cliOptions, home string) (config.File, error) {
	path := options.configPath
	if path == "" {
		path = filepath.Join(home, ".config", "otto", "config.toml")
	}
	file, err := config.Load(path)
	if err != nil && !options.explicitConfig && os.IsNotExist(err) {
		return config.File{}, nil
	}
	return file, err
}

func configEnvironment(file config.File, getenv func(string) string) map[string]string {
	keys := map[string]struct{}{
		"OTTO_PROVIDER": {},
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
		environment[key] = getenv(key)
	}
	return environment
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

func validateShell(path string) error {
	info, err := os.Stat(path)
	if err != nil {
		return fmt.Errorf("invalid shell %q: %w", path, err)
	}
	if info.IsDir() || info.Mode().Perm()&0o111 == 0 {
		return fmt.Errorf("invalid shell %q: not executable", path)
	}
	return nil
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
