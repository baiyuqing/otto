package app

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"path/filepath"
	"runtime"
	"strings"
	"sync"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/memory"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

var (
	ErrPromptActive        = errors.New("prompt already active")
	ErrClosed              = errors.New("controller is closed")
	ErrPersistenceDisabled = errors.New("session persistence is disabled")
	ErrMemoryUnavailable   = errors.New("memory is not available")
)

type Runner interface {
	Run(context.Context, string, func(agent.Event)) error
	Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error)
}

type SessionFactory func() (session.Session, error)

type RunnerFactory func(session.Session) Runner

type SessionLister func(context.Context, int) (session.ListResult, error)

type ResumeFactory func(context.Context, string) (SessionReplacement, error)

type NewSessionBuilder func(context.Context, RuntimeInfo) (SessionReplacement, error)

type ArchiveFactory func(context.Context, string) (session.ArchiveResult, error)

type RuntimeInfo struct {
	Provider      string
	Profile       string
	Model         string
	ContextWindow int
}

type SessionReplacement struct {
	Session     session.Session
	Runner      Runner
	RuntimeInfo RuntimeInfo
	Warnings    []session.Warning
}

type ResumeResult struct {
	SessionPath string
	Warnings    []session.Warning
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

func WithNewSessionBuilder(build NewSessionBuilder) Option {
	return func(controller *Controller) {
		controller.newSession = build
	}
}

func WithSessionArchiver(archive ArchiveFactory) Option {
	return func(controller *Controller) {
		controller.archiveSession = archive
	}
}

func WithMemory(manager memory.Manager, userScope, workspaceScope memory.Scope) Option {
	return func(controller *Controller) {
		controller.memoryManager = manager
		controller.memoryUserScope = userScope
		controller.memoryWorkspaceScope = workspaceScope
	}
}

type Info struct {
	SessionID                 string
	SessionPath               string
	Workspace                 string
	Provider                  string
	Profile                   string
	Model                     string
	Usage                     model.Usage
	UsagePresent              bool
	ContextWindow             int
	ContextInputTokens        int
	ContextInputTokensPresent bool
	ContextInputTokensPending bool
}

type Backend interface {
	Prompt(context.Context, string, func(agent.Event)) error
	Compact(context.Context, string, func(agent.Event)) (agent.CompactionResult, error)
	NewSession() error
	Info() Info
	History() []model.Message
}

type SessionBrowser interface {
	ListSessions(context.Context, int) (session.ListResult, error)
	ResumeSession(context.Context, string) (ResumeResult, error)
}

type SessionArchiver interface {
	SessionBrowser
	ArchiveSession(context.Context, string) (session.ArchiveResult, error)
	ArchiveCurrentSession(context.Context) (session.ArchiveResult, error)
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
	buildActive       bool
	current           session.Session
	currentWorkspace  string
	runner            Runner
	replacement       session.Session
	replacementClosed bool
	closeRequested    bool
}

// closeRunner closes runner if it implements io.Closer. The Runner interface
// stays narrow (Run, Compact) because most implementations own no closable
// resources; a memory-bound agent.Agent is the one that does.
func closeRunner(runner Runner) error {
	if closer, ok := runner.(io.Closer); ok {
		return closer.Close()
	}
	return nil
}

// joinCloseErrors combines two close errors without disturbing callers that
// compare a lone error by identity (errors.Join always allocates, even for
// one non-nil argument).
func joinCloseErrors(a, b error) error {
	if a == nil {
		return b
	}
	if b == nil {
		return a
	}
	return errors.Join(a, b)
}

type activeOperation struct {
	done           chan struct{}
	owner          uint64
	runner         Runner
	current        session.Session
	callbackDepth  int
	closeRequested bool
}

type Controller struct {
	mu                   sync.Mutex
	current              session.Session
	currentPath          string
	currentWorkspace     string
	runner               Runner
	create               SessionFactory
	build                RunnerFactory
	listSessions         SessionLister
	resumeSession        ResumeFactory
	newSession           NewSessionBuilder
	archiveSession       ArchiveFactory
	memoryManager        memory.Manager
	memoryUserScope      memory.Scope
	memoryWorkspaceScope memory.Scope
	active               *activeOperation
	replace              *replacementState
	closed               bool
	closeDone            chan struct{}
	closeComplete        bool
	closeErr             error
	reentrantCloseOwner  uint64
	ownerIDSource        func() uint64
	runtimeInfo          *RuntimeInfo
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
		ownerIDSource:    currentGoroutineID,
	}
	for _, option := range options {
		if option != nil {
			option(controller)
		}
	}
	return controller, nil
}

func (c *Controller) Prompt(ctx context.Context, text string, emit func(agent.Event)) error {
	operation, err := c.beginOperation()
	if err != nil {
		return err
	}
	defer c.endOperation(operation)
	return operation.runner.Run(ctx, text, c.operationCallback(operation, emit))
}

func (c *Controller) Compact(ctx context.Context, focus string, emit func(agent.Event)) (agent.CompactionResult, error) {
	operation, err := c.beginOperation()
	if err != nil {
		return agent.CompactionResult{}, err
	}
	defer c.endOperation(operation)
	return operation.runner.Compact(ctx, focus, c.operationCallback(operation, emit))
}

func (c *Controller) beginOperation() (*activeOperation, error) {
	owner := c.ownerID()
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return nil, ErrClosed
	}
	if c.active != nil || c.replace != nil {
		return nil, ErrPromptActive
	}
	operation := &activeOperation{
		done:    make(chan struct{}),
		owner:   owner,
		runner:  c.runner,
		current: c.current,
	}
	c.active = operation
	return operation, nil
}

func (c *Controller) operationCallback(operation *activeOperation, emit func(agent.Event)) func(agent.Event) {
	if emit == nil {
		return nil
	}
	return func(event agent.Event) {
		c.mu.Lock()
		if c.active == operation {
			operation.callbackDepth++
		}
		c.mu.Unlock()

		defer func() {
			c.mu.Lock()
			if c.active == operation && operation.callbackDepth > 0 {
				operation.callbackDepth--
			}
			c.mu.Unlock()
		}()
		emit(event)
	}
}

func (c *Controller) endOperation(operation *activeOperation) {
	c.mu.Lock()
	if c.active != operation {
		c.mu.Unlock()
		return
	}
	if !operation.closeRequested {
		c.active = nil
		close(operation.done)
		c.mu.Unlock()
		return
	}
	current := operation.current
	closeDone := c.closeDone
	c.mu.Unlock()

	var closeErr error
	if current != nil {
		closeErr = current.Close()
	}
	closeErr = joinCloseErrors(closeErr, closeRunner(operation.runner))

	c.mu.Lock()
	if c.active == operation {
		c.active = nil
		close(operation.done)
	}
	c.mu.Unlock()
	c.completeClose(closeDone, closeErr)
}

func (c *Controller) NewSession() error {
	owner := c.ownerID()
	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return ErrClosed
	}
	if c.active != nil || c.replace != nil {
		c.mu.Unlock()
		return ErrPromptActive
	}
	state := c.beginReplacementLocked(owner)
	current := c.current
	var runtimeInfo *RuntimeInfo
	if c.runtimeInfo != nil {
		copy := *c.runtimeInfo
		runtimeInfo = &copy
	}
	c.mu.Unlock()

	if runtimeInfo == nil {
		header := current.Header()
		runtimeInfo = &RuntimeInfo{Provider: header.Provider, Profile: header.Profile, Model: header.Model}
	}

	_, err := c.runReplacement(context.Background(), state, func() (SessionReplacement, error) {
		return c.buildReplacement(context.Background(), runtimeInfo)
	}, true, false)
	return err
}

func (c *Controller) buildReplacement(ctx context.Context, runtimeInfo *RuntimeInfo) (SessionReplacement, error) {
	builder := c.newSession
	if builder != nil {
		return builder(ctx, *runtimeInfo)
	}
	replacement, createErr := c.create()
	if createErr != nil {
		return SessionReplacement{Session: replacement}, createErr
	}
	if replacement == nil {
		return SessionReplacement{}, errors.New("session factory returned nil session")
	}
	runner := c.build(replacement)
	if runner == nil {
		return SessionReplacement{Session: replacement}, errors.New("runner factory returned nil runner")
	}
	header := replacement.Header()
	return SessionReplacement{
		Session: replacement,
		Runner:  runner,
		RuntimeInfo: RuntimeInfo{
			Provider:      header.Provider,
			Profile:       header.Profile,
			Model:         header.Model,
			ContextWindow: runtimeInfo.ContextWindow,
		},
	}, nil
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
	owner := c.ownerID()

	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return ResumeResult{}, ErrClosed
	}
	if c.active != nil || c.replace != nil {
		c.mu.Unlock()
		return ResumeResult{}, ErrPromptActive
	}
	factory := c.resumeSession
	if factory == nil {
		c.mu.Unlock()
		return ResumeResult{}, ErrPersistenceDisabled
	}
	if requestedPath != "" && requestedPath == c.currentPath {
		result := ResumeResult{SessionPath: strings.Clone(c.currentPath)}
		c.mu.Unlock()
		return result, nil
	}
	state := c.beginReplacementLocked(owner)
	c.mu.Unlock()

	return c.runReplacement(ctx, state, func() (SessionReplacement, error) {
		return factory(ctx, path)
	}, true, true)
}

// ArchiveSession archives an active session file without touching the current
// session state. Selecting the current session delegates to ArchiveCurrentSession
// so picker "current" rows behave identically to /archive on the current session.
func (c *Controller) ArchiveSession(ctx context.Context, path string) (session.ArchiveResult, error) {
	requestedPath := canonicalSessionPath(path)

	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return session.ArchiveResult{}, ErrClosed
	}
	if c.active != nil || c.replace != nil {
		c.mu.Unlock()
		return session.ArchiveResult{}, ErrPromptActive
	}
	factory := c.archiveSession
	if factory == nil {
		c.mu.Unlock()
		return session.ArchiveResult{}, ErrPersistenceDisabled
	}
	currentPath := c.currentPath
	c.mu.Unlock()

	if requestedPath != "" && requestedPath == currentPath {
		return c.ArchiveCurrentSession(ctx)
	}
	if err := ctx.Err(); err != nil {
		return session.ArchiveResult{}, err
	}
	return factory(ctx, path)
}

// ArchiveCurrentSession archives the current session file and starts a fresh
// session in its place. The fresh session is built before the archive move so
// every failure path leaves the current session fully intact; the only committed
// state change is the atomic file move, which happens last.
func (c *Controller) ArchiveCurrentSession(ctx context.Context) (session.ArchiveResult, error) {
	owner := c.ownerID()

	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return session.ArchiveResult{}, ErrClosed
	}
	if c.active != nil || c.replace != nil {
		c.mu.Unlock()
		return session.ArchiveResult{}, ErrPromptActive
	}
	factory := c.archiveSession
	if factory == nil {
		c.mu.Unlock()
		return session.ArchiveResult{}, ErrPersistenceDisabled
	}
	current := c.current
	currentPath := c.currentPath
	if currentPath == "" {
		c.mu.Unlock()
		return session.ArchiveResult{}, ErrPersistenceDisabled
	}
	state := c.beginReplacementLocked(owner)
	var runtimeInfo *RuntimeInfo
	if c.runtimeInfo != nil {
		copy := *c.runtimeInfo
		runtimeInfo = &copy
	}
	c.mu.Unlock()

	if runtimeInfo == nil {
		header := current.Header()
		runtimeInfo = &RuntimeInfo{Provider: header.Provider, Profile: header.Profile, Model: header.Model}
	}

	var archiveResult session.ArchiveResult
	_, err := c.runReplacement(ctx, state, func() (SessionReplacement, error) {
		replacement, buildErr := c.buildReplacement(ctx, runtimeInfo)
		if buildErr != nil {
			return SessionReplacement{}, buildErr
		}
		archive, archiveErr := factory(ctx, currentPath)
		if archiveErr != nil {
			return SessionReplacement{Session: replacement.Session}, archiveErr
		}
		archiveResult = archive
		return replacement, nil
	}, true, false)
	if err != nil {
		return session.ArchiveResult{}, err
	}
	return archiveResult, nil
}

// SearchMemory, RememberMemory, ForgetMemory, and ReviewMemoryCandidate
// delegate straight to the injected memory.Manager. The manager owns its own
// concurrency (store-level locking), so these do not touch c.mu's
// replacement state machine.
func (c *Controller) SearchMemory(ctx context.Context, request memory.SearchRequest) (memory.SearchResult, error) {
	c.mu.Lock()
	manager := c.memoryManager
	c.mu.Unlock()
	if manager == nil {
		return memory.SearchResult{}, ErrMemoryUnavailable
	}
	return manager.Search(ctx, request)
}

func (c *Controller) RememberMemory(ctx context.Context, request memory.RememberRequest) (memory.Record, error) {
	c.mu.Lock()
	manager := c.memoryManager
	c.mu.Unlock()
	if manager == nil {
		return memory.Record{}, ErrMemoryUnavailable
	}
	return manager.Remember(ctx, request)
}

func (c *Controller) ForgetMemory(ctx context.Context, request memory.ForgetRequest) (memory.ForgetResult, error) {
	c.mu.Lock()
	manager := c.memoryManager
	c.mu.Unlock()
	if manager == nil {
		return memory.ForgetResult{}, ErrMemoryUnavailable
	}
	return manager.Forget(ctx, request)
}

func (c *Controller) ReviewMemoryCandidate(ctx context.Context, request memory.ReviewRequest) (memory.ReviewResult, error) {
	c.mu.Lock()
	manager := c.memoryManager
	c.mu.Unlock()
	if manager == nil {
		return memory.ReviewResult{}, ErrMemoryUnavailable
	}
	return manager.Review(ctx, request)
}

// MemoryScopes returns the user and workspace scopes the bound memory
// manager was configured with, and whether a manager is bound at all.
func (c *Controller) MemoryScopes() (memory.Scope, memory.Scope, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.memoryManager == nil {
		return memory.Scope{}, memory.Scope{}, false
	}
	return c.memoryUserScope, c.memoryWorkspaceScope, true
}

func (c *Controller) GetMemory(ctx context.Context, ref memory.RecordRef) (memory.Record, error) {
	c.mu.Lock()
	manager := c.memoryManager
	c.mu.Unlock()
	if manager == nil {
		return memory.Record{}, ErrMemoryUnavailable
	}
	return manager.Get(ctx, ref)
}

func (c *Controller) beginReplacementLocked(owner uint64) *replacementState {
	state := &replacementState{
		done:             make(chan struct{}),
		owner:            owner,
		phase:            replacementPhaseBuilding,
		current:          c.current,
		currentWorkspace: c.currentWorkspace,
		runner:           c.runner,
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

	c.mu.Lock()
	if c.replace == state {
		state.buildActive = true
	}
	c.mu.Unlock()
	replacement, err := build()
	c.mu.Lock()
	if c.replace == state {
		state.buildActive = false
	}
	c.mu.Unlock()
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
		c.mu.Unlock()
		c.abortReplacement(state, replacement.Session)
		return ResumeResult{}, ErrClosed
	case cancelErr != nil || channelClosed(cancelDone):
		c.mu.Unlock()
		c.abortReplacement(state, replacement.Session)
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

	if err := joinCloseErrors(state.current.Close(), closeRunner(state.runner)); err != nil {
		c.mu.Lock()
		deferredClose := state.closeRequested
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
	c.currentPath = strings.Clone(replacementPath)
	committedPath := strings.Clone(c.currentPath)
	c.currentWorkspace = replacementWorkspace
	c.runner = replacement.Runner
	if replaceRuntime {
		runtimeInfo := replacement.RuntimeInfo
		c.runtimeInfo = &runtimeInfo
	}
	closed := c.closed
	deferredClose := state.closeRequested
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
	return ResumeResult{SessionPath: committedPath, Warnings: warnings}, nil
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
	shouldClose := replacement != nil && !state.replacementClosed
	if shouldClose {
		state.replacementClosed = true
		if c.replace == state {
			state.phase = replacementPhaseCleaning
		}
	}
	c.mu.Unlock()

	if shouldClose {
		_ = replacement.Close()
	}

	c.mu.Lock()
	if c.replace != state {
		c.mu.Unlock()
		return
	}
	if !state.closeRequested {
		c.finishReplacingLocked(state)
		c.mu.Unlock()
		return
	}
	state.phase = replacementPhaseClosingCurrent
	current := state.current
	closeDone := c.closeDone
	c.mu.Unlock()

	var closeErr error
	if current != nil {
		closeErr = current.Close()
	}
	closeErr = joinCloseErrors(closeErr, closeRunner(state.runner))
	c.finishReplacing(state)
	c.completeClose(closeDone, closeErr)
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
	currentPath := strings.Clone(c.currentPath)
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
		SessionPath: currentPath,
		Workspace:   header.Workspace,
		Provider:    header.Provider,
		Profile:     header.Profile,
		Model:       header.Model,
	}
	if runtimeInfo != nil {
		info.Provider = runtimeInfo.Provider
		info.Profile = runtimeInfo.Profile
		info.Model = runtimeInfo.Model
		info.ContextWindow = runtimeInfo.ContextWindow
	}
	if snapshotSource, ok := current.(session.SnapshotProvider); ok {
		snapshot := snapshotSource.Snapshot()
		info.Usage = snapshot.AggregateUsage
		info.UsagePresent = snapshot.AggregateUsagePresent
		info.ContextInputTokens = snapshot.ContextInputTokens
		info.ContextInputTokensPresent = snapshot.ContextInputTokensPresent
		info.ContextInputTokensPending = snapshot.ContextInputTokensPending
		return info
	}
	if usageSource, ok := current.(session.UsageProvider); ok {
		info.Usage, info.UsagePresent = usageSource.AggregateUsage()
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
	caller := c.ownerID()
	c.mu.Lock()
	if c.closeDone != nil {
		done := c.closeDone
		if c.closeComplete || c.activeCloseIsReentrantLocked(caller) || c.replacementCloseIsReentrantLocked(caller) || caller != 0 && c.reentrantCloseOwner == caller {
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

	if c.active != nil {
		operation := c.active
		operation.closeRequested = true
		if c.activeCloseIsReentrantLocked(caller) {
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

	if c.replace != nil {
		state := c.replace
		state.closeRequested = true
		if c.replacementCloseIsReentrantLocked(caller) {
			if caller != 0 {
				c.reentrantCloseOwner = caller
			}
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

	if c.current == nil && c.closeErr != nil {
		err := c.closeErr
		c.mu.Unlock()
		c.completeClose(done, err)
		return err
	}
	current := c.current
	runner := c.runner
	c.mu.Unlock()

	var err error
	if current != nil {
		err = current.Close()
	}
	err = joinCloseErrors(err, closeRunner(runner))
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

func (c *Controller) activeCloseIsReentrantLocked(caller uint64) bool {
	if c.active == nil {
		return false
	}
	if caller != 0 {
		return c.active.owner == caller
	}
	return c.active.owner == 0 && c.active.callbackDepth > 0
}

func (c *Controller) replacementCloseIsReentrantLocked(caller uint64) bool {
	if c.replace == nil {
		return false
	}
	if caller != 0 {
		return c.replace.owner == caller
	}
	return c.replace.owner == 0 && c.replace.buildActive
}

func (c *Controller) ownerID() uint64 {
	if c.ownerIDSource == nil {
		return currentGoroutineID()
	}
	return c.ownerIDSource()
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
