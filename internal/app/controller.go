package app

import (
	"context"
	"encoding/json"
	"errors"
	"path/filepath"
	"runtime"
	"sync"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

var (
	ErrPromptActive        = errors.New("prompt already active")
	ErrClosed              = errors.New("controller is closed")
	ErrPersistenceDisabled = errors.New("session persistence is disabled")
)

type Runner interface {
	Run(context.Context, string, func(agent.Event)) error
}

type SessionFactory func() (session.Session, error)

type RunnerFactory func(session.Session) Runner

type SessionLister func(context.Context, int) (session.ListResult, error)

type ResumeFactory func(context.Context, string) (SessionReplacement, error)

type RuntimeInfo struct {
	Provider string
	Profile  string
	Model    string
}

type SessionReplacement struct {
	Session     session.Session
	Runner      Runner
	RuntimeInfo RuntimeInfo
	Warnings    []session.Warning
}

type ResumeResult struct {
	Warnings []session.Warning
}

type Option func(*Controller)

func WithRuntimeInfo(info RuntimeInfo) Option {
	return func(controller *Controller) {
		copy := info
		controller.runtimeInfo = &copy
	}
}

func WithSessionBrowser(list SessionLister, resume ResumeFactory) Option {
	return func(controller *Controller) {
		controller.listSessions = list
		controller.resumeSession = resume
	}
}

type Info struct {
	SessionID   string
	SessionPath string
	Workspace   string
	Provider    string
	Profile     string
	Model       string
}

type Backend interface {
	Prompt(context.Context, string, func(agent.Event)) error
	NewSession() error
	Info() Info
	History() []model.Message
}

type SessionBrowser interface {
	ListSessions(context.Context, int) (session.ListResult, error)
	ResumeSession(context.Context, string) (ResumeResult, error)
}

type replacementPhase uint8

const (
	replacementPhaseBuilding replacementPhase = iota + 1
	replacementPhaseClosingCurrent
	replacementPhaseCleaning
)

type replacementState struct {
	done              chan struct{}
	owner             uint64
	phase             replacementPhase
	current           session.Session
	currentWorkspace  string
	replacement       session.Session
	replacementClosed bool
	closeAfterSwap    bool
}

type Controller struct {
	mu                  sync.Mutex
	current             session.Session
	currentPath         string
	currentWorkspace    string
	runner              Runner
	create              SessionFactory
	build               RunnerFactory
	listSessions        SessionLister
	resumeSession       ResumeFactory
	prompting           bool
	replace             *replacementState
	closed              bool
	activeDone          chan struct{}
	closeDone           chan struct{}
	closeComplete       bool
	closeErr            error
	reentrantCloseOwner uint64
	runtimeInfo         *RuntimeInfo
}

func New(initial session.Session, create SessionFactory, build RunnerFactory, options ...Option) (*Controller, error) {
	if initial == nil {
		return nil, errors.New("initial session is required")
	}
	if create == nil {
		return nil, errors.New("session factory is required")
	}
	if build == nil {
		return nil, errors.New("runner factory is required")
	}
	runner := build(initial)
	if runner == nil {
		return nil, errors.New("runner factory returned nil runner")
	}

	path := canonicalSessionPath(initial.Path())
	workspace := canonicalSessionPath(initial.Header().Workspace)
	controller := &Controller{
		current:          initial,
		currentPath:      path,
		currentWorkspace: workspace,
		runner:           runner,
		create:           create,
		build:            build,
	}
	for _, option := range options {
		if option != nil {
			option(controller)
		}
	}
	return controller, nil
}

func (c *Controller) Prompt(ctx context.Context, text string, emit func(agent.Event)) error {
	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return ErrClosed
	}
	if c.prompting || c.replace != nil {
		c.mu.Unlock()
		return ErrPromptActive
	}
	runner := c.runner
	done := make(chan struct{})
	c.prompting = true
	c.activeDone = done
	c.mu.Unlock()

	defer func() {
		c.mu.Lock()
		c.prompting = false
		if c.activeDone == done {
			c.activeDone = nil
			close(done)
		}
		c.mu.Unlock()
	}()

	return runner.Run(ctx, text, emit)
}

func (c *Controller) NewSession() error {
	state, err := c.beginReplacement()
	if err != nil {
		return err
	}

	_, err = c.runReplacement(context.Background(), state, func() (SessionReplacement, error) {
		replacement, err := c.create()
		if err != nil {
			return SessionReplacement{Session: replacement}, err
		}
		if replacement == nil {
			return SessionReplacement{}, errors.New("session factory returned nil session")
		}
		runner := c.build(replacement)
		if runner == nil {
			return SessionReplacement{Session: replacement}, errors.New("runner factory returned nil runner")
		}
		return SessionReplacement{Session: replacement, Runner: runner}, nil
	}, false, false)
	return err
}

func (c *Controller) ListSessions(ctx context.Context, limit int) (session.ListResult, error) {
	c.mu.Lock()
	list := c.listSessions
	currentPath := c.currentPath
	c.mu.Unlock()
	if list == nil {
		return session.ListResult{}, ErrPersistenceDisabled
	}

	result, err := list(ctx, limit)
	if err != nil {
		return session.ListResult{}, err
	}
	result.Sessions = cloneSessionInfos(result.Sessions)
	for index := range result.Sessions {
		result.Sessions[index].Current = currentPath != "" && canonicalSessionPath(result.Sessions[index].Path) == currentPath
	}
	return result, nil
}

func (c *Controller) ResumeSession(ctx context.Context, path string) (ResumeResult, error) {
	requestedPath := canonicalSessionPath(path)
	owner := currentGoroutineID()

	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return ResumeResult{}, ErrClosed
	}
	if c.prompting || c.replace != nil {
		c.mu.Unlock()
		return ResumeResult{}, ErrPromptActive
	}
	factory := c.resumeSession
	if factory == nil {
		c.mu.Unlock()
		return ResumeResult{}, ErrPersistenceDisabled
	}
	if requestedPath != "" && requestedPath == c.currentPath {
		c.mu.Unlock()
		return ResumeResult{}, nil
	}
	state := c.beginReplacementLocked(owner)
	c.mu.Unlock()

	return c.runReplacement(ctx, state, func() (SessionReplacement, error) {
		return factory(ctx, path)
	}, true, true)
}

func (c *Controller) beginReplacement() (*replacementState, error) {
	owner := currentGoroutineID()
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return nil, ErrClosed
	}
	if c.prompting || c.replace != nil {
		return nil, ErrPromptActive
	}
	return c.beginReplacementLocked(owner), nil
}

func (c *Controller) beginReplacementLocked(owner uint64) *replacementState {
	state := &replacementState{
		done:             make(chan struct{}),
		owner:            owner,
		phase:            replacementPhaseBuilding,
		current:          c.current,
		currentWorkspace: c.currentWorkspace,
	}
	c.replace = state
	return state
}

func (c *Controller) runReplacement(
	ctx context.Context,
	state *replacementState,
	build func() (SessionReplacement, error),
	replaceRuntime bool,
	validateWorkspace bool,
) (ResumeResult, error) {
	if err := ctx.Err(); err != nil {
		c.abortReplacement(state, nil)
		return ResumeResult{}, err
	}

	replacement, err := build()
	warnings := cloneWarnings(replacement.Warnings)
	if err != nil {
		c.abortReplacement(state, replacement.Session)
		return ResumeResult{}, err
	}
	if replacement.Session == nil {
		c.abortReplacement(state, nil)
		return ResumeResult{}, errors.New("resume factory returned nil session")
	}
	if replacement.Runner == nil {
		c.abortReplacement(state, replacement.Session)
		return ResumeResult{}, errors.New("resume factory returned nil runner")
	}

	if !c.registerReplacement(state, replacement.Session) {
		c.abortReplacement(state, replacement.Session)
		return ResumeResult{}, ErrClosed
	}

	replacementPath := replacement.Session.Path()
	if validateWorkspace && replacementPath == "" {
		c.abortReplacement(state, replacement.Session)
		return ResumeResult{}, errors.New("replacement session path is required")
	}
	if replacementPath != "" {
		replacementPath = canonicalSessionPath(replacementPath)
	}
	replacementWorkspace := canonicalSessionPath(replacement.Session.Header().Workspace)
	if validateWorkspace && replacementWorkspace != state.currentWorkspace {
		c.abortReplacement(state, replacement.Session)
		return ResumeResult{}, errors.New("replacement session workspace does not match current workspace")
	}

	cancelDone := ctx.Done()
	cancelErr := ctx.Err()
	c.mu.Lock()
	switch {
	case c.replace != state || c.closed:
		shouldClose := c.releaseReplacementLocked(state, replacement.Session)
		c.mu.Unlock()
		if shouldClose {
			_ = replacement.Session.Close()
			c.finishReplacing(state)
		}
		return ResumeResult{}, ErrClosed
	case cancelErr != nil || channelClosed(cancelDone):
		shouldClose := c.releaseReplacementLocked(state, replacement.Session)
		c.mu.Unlock()
		if shouldClose {
			_ = replacement.Session.Close()
			c.finishReplacing(state)
		}
		if cancelErr == nil {
			cancelErr = ctx.Err()
			if cancelErr == nil {
				cancelErr = context.Canceled
			}
		}
		return ResumeResult{}, cancelErr
	default:
		state.phase = replacementPhaseClosingCurrent
		c.mu.Unlock()
	}

	if err := state.current.Close(); err != nil {
		c.mu.Lock()
		deferredClose := state.closeAfterSwap
		shouldClose := c.releaseReplacementLocked(state, replacement.Session)
		closeDone, completeClose := c.finishClosedLocked(err, deferredClose)
		c.mu.Unlock()
		if shouldClose {
			_ = replacement.Session.Close()
			c.finishReplacing(state)
		}
		if completeClose {
			c.completeClose(closeDone, err)
		}
		return ResumeResult{}, err
	}

	c.mu.Lock()
	if c.replace != state {
		shouldClose := !state.replacementClosed
		state.replacementClosed = true
		closed := c.closed
		c.mu.Unlock()
		if shouldClose {
			_ = replacement.Session.Close()
		}
		if closed {
			return ResumeResult{}, ErrClosed
		}
		return ResumeResult{}, ErrClosed
	}
	c.current = replacement.Session
	c.currentPath = replacementPath
	c.currentWorkspace = replacementWorkspace
	c.runner = replacement.Runner
	if replaceRuntime {
		runtimeInfo := replacement.RuntimeInfo
		c.runtimeInfo = &runtimeInfo
	}
	closed := c.closed
	deferredClose := state.closeAfterSwap
	closeDone := c.closeDone
	if deferredClose {
		state.replacementClosed = true
		state.phase = replacementPhaseCleaning
	} else {
		c.finishReplacingLocked(state)
	}
	c.mu.Unlock()

	if deferredClose {
		closeErr := replacement.Session.Close()
		c.finishReplacing(state)
		c.completeClose(closeDone, closeErr)
	}
	if closed {
		return ResumeResult{}, ErrClosed
	}
	return ResumeResult{Warnings: warnings}, nil
}

func (c *Controller) registerReplacement(state *replacementState, replacement session.Session) bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.replace != state || c.closed {
		return false
	}
	state.replacement = replacement
	return true
}

func (c *Controller) abortReplacement(state *replacementState, replacement session.Session) {
	c.mu.Lock()
	shouldClose := c.releaseReplacementLocked(state, replacement)
	c.mu.Unlock()
	if shouldClose {
		_ = replacement.Close()
		c.finishReplacing(state)
	}
}

func (c *Controller) releaseReplacementLocked(state *replacementState, replacement session.Session) bool {
	shouldClose := replacement != nil && !state.replacementClosed
	if shouldClose {
		state.replacementClosed = true
		if c.replace == state {
			state.phase = replacementPhaseCleaning
		}
		return true
	}
	c.finishReplacingLocked(state)
	return false
}

func (c *Controller) Info() Info {
	c.mu.Lock()
	current := c.current
	var runtimeInfo *RuntimeInfo
	if c.runtimeInfo != nil {
		copy := *c.runtimeInfo
		runtimeInfo = &copy
	}
	c.mu.Unlock()
	if current == nil {
		return Info{}
	}
	header := current.Header()
	info := Info{
		SessionID:   header.ID,
		SessionPath: current.Path(),
		Workspace:   header.Workspace,
		Provider:    header.Provider,
		Profile:     header.Profile,
		Model:       header.Model,
	}
	if runtimeInfo != nil {
		info.Provider = runtimeInfo.Provider
		info.Profile = runtimeInfo.Profile
		info.Model = runtimeInfo.Model
	}
	return info
}

func (c *Controller) History() []model.Message {
	c.mu.Lock()
	current := c.current
	c.mu.Unlock()
	if current == nil {
		return nil
	}
	return cloneMessages(current.Messages())
}

func (c *Controller) Close() error {
	caller := currentGoroutineID()
	c.mu.Lock()
	if c.closeDone != nil {
		done := c.closeDone
		if c.closeComplete || caller != 0 && (c.replacementOwnedByLocked(caller) || c.reentrantCloseOwner == caller) {
			err := c.closeErr
			c.mu.Unlock()
			return err
		}
		c.mu.Unlock()
		<-done
		c.mu.Lock()
		err := c.closeErr
		c.mu.Unlock()
		return err
	}

	c.closed = true
	done := make(chan struct{})
	c.closeDone = done
	activeDone := c.activeDone

	if caller != 0 && c.replace != nil && c.replace.owner == caller {
		state := c.replace
		c.reentrantCloseOwner = caller
		if state.phase == replacementPhaseClosingCurrent {
			state.closeAfterSwap = true
			c.mu.Unlock()
			return nil
		}

		current := c.current
		replacement := state.replacement
		cleaning := state.phase == replacementPhaseCleaning
		if replacement != nil {
			state.replacementClosed = true
		}
		c.finishReplacingLocked(state)
		c.mu.Unlock()

		var err error
		if current != nil {
			err = current.Close()
		}
		if replacement != nil && !cleaning {
			if closeErr := replacement.Close(); err == nil {
				err = closeErr
			}
		}
		c.completeClose(done, err)
		return err
	}

	var replaceDone chan struct{}
	if c.replace != nil {
		replaceDone = c.replace.done
	}
	c.mu.Unlock()

	if activeDone != nil {
		<-activeDone
	}
	if replaceDone != nil {
		<-replaceDone
	}

	c.mu.Lock()
	if c.current == nil && c.closeErr != nil {
		err := c.closeErr
		c.mu.Unlock()
		c.completeClose(done, err)
		return err
	}
	current := c.current
	c.mu.Unlock()

	var err error
	if current != nil {
		err = current.Close()
	}
	c.completeClose(done, err)
	return err
}

func (c *Controller) finishClosedLocked(err error, deferredClose bool) (chan struct{}, bool) {
	c.closed = true
	c.current = nil
	c.currentPath = ""
	c.currentWorkspace = ""
	c.runner = nil
	if c.closeErr == nil {
		c.closeErr = err
	}
	if c.closeDone == nil {
		c.closeDone = make(chan struct{})
		return c.closeDone, true
	}
	return c.closeDone, deferredClose
}

func (c *Controller) completeClose(done chan struct{}, err error) {
	c.mu.Lock()
	if c.closeErr == nil {
		c.closeErr = err
	}
	if c.closeDone == done && !c.closeComplete {
		c.closeComplete = true
		close(done)
	}
	c.mu.Unlock()
}

func (c *Controller) finishReplacing(state *replacementState) {
	c.mu.Lock()
	c.finishReplacingLocked(state)
	c.mu.Unlock()
}

func (c *Controller) finishReplacingLocked(state *replacementState) {
	if c.replace != state {
		return
	}
	c.replace = nil
	close(state.done)
}

func (c *Controller) replacementOwnedByLocked(owner uint64) bool {
	return c.replace != nil && c.replace.owner == owner
}

// currentGoroutineID supplies the execution identity needed to distinguish a
// synchronous replacement callback from another controller's callback. Close
// cannot accept an ownership token without breaking its public lifecycle API,
// and the runtime does not expose a supported goroutine-local identity API.
func currentGoroutineID() uint64 {
	var stack [64]byte
	n := runtime.Stack(stack[:], false)
	const prefix = "goroutine "
	if n <= len(prefix) || string(stack[:len(prefix)]) != prefix {
		return 0
	}

	var id uint64
	for _, character := range stack[len(prefix):n] {
		if character < '0' || character > '9' {
			break
		}
		id = id*10 + uint64(character-'0')
	}
	return id
}

func channelClosed(done <-chan struct{}) bool {
	if done == nil {
		return false
	}
	select {
	case <-done:
		return true
	default:
		return false
	}
}

func canonicalSessionPath(path string) string {
	if path == "" {
		return ""
	}
	absolute, err := filepath.Abs(path)
	if err != nil {
		return filepath.Clean(path)
	}
	canonical, err := filepath.EvalSymlinks(absolute)
	if err == nil {
		return filepath.Clean(canonical)
	}
	return filepath.Clean(absolute)
}

func cloneSessionInfos(infos []session.SessionInfo) []session.SessionInfo {
	if infos == nil {
		return nil
	}
	return append([]session.SessionInfo(nil), infos...)
}

func cloneWarnings(warnings []session.Warning) []session.Warning {
	if warnings == nil {
		return nil
	}
	return append([]session.Warning(nil), warnings...)
}

func cloneMessages(messages []model.Message) []model.Message {
	if messages == nil {
		return nil
	}
	cloned := make([]model.Message, len(messages))
	for i, message := range messages {
		cloned[i] = cloneMessage(message)
	}
	return cloned
}

func cloneMessage(message model.Message) model.Message {
	cloned := message
	if message.Blocks != nil {
		cloned.Blocks = make([]model.Block, len(message.Blocks))
		for i, block := range message.Blocks {
			cloned.Blocks[i] = block
			cloned.Blocks[i].Arguments = cloneArguments(block.Arguments)
		}
	}
	if message.Usage != nil {
		usage := *message.Usage
		cloned.Usage = &usage
	}
	return cloned
}

func cloneArguments(arguments json.RawMessage) json.RawMessage {
	if arguments == nil {
		return nil
	}
	return append(json.RawMessage(nil), arguments...)
}
