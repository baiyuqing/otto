package app

import (
	"context"
	"encoding/json"
	"errors"
	"runtime"
	"sync"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

var (
	ErrPromptActive = errors.New("prompt already active")
	ErrClosed       = errors.New("controller is closed")
)

const controllerNewSessionFunc = "github.com/baiyuqing/otto/internal/app.(*Controller).NewSession"

type Runner interface {
	Run(context.Context, string, func(agent.Event)) error
}

type SessionFactory func() (session.Session, error)

type RunnerFactory func(session.Session) Runner

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

type replacementPhase uint8

const (
	replacementPhaseCreating replacementPhase = iota + 1
	replacementPhaseBuilding
	replacementPhaseClosingCurrent
)

type replacementState struct {
	done              chan struct{}
	phase             replacementPhase
	replacement       session.Session
	replacementClosed bool
}

type Controller struct {
	mu         sync.Mutex
	current    session.Session
	runner     Runner
	create     SessionFactory
	build      RunnerFactory
	prompting  bool
	replace    *replacementState
	closed     bool
	activeDone chan struct{}
	closeDone  chan struct{}
	closeErr   error
}

func New(initial session.Session, create SessionFactory, build RunnerFactory) (*Controller, error) {
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
	return &Controller{current: initial, runner: runner, create: create, build: build}, nil
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
	c.mu.Lock()
	if c.closed {
		c.mu.Unlock()
		return ErrClosed
	}
	if c.prompting || c.replace != nil {
		c.mu.Unlock()
		return ErrPromptActive
	}
	state := &replacementState{done: make(chan struct{}), phase: replacementPhaseCreating}
	c.replace = state
	current := c.current
	c.mu.Unlock()

	replacement, err := c.create()
	if err != nil {
		c.finishReplacing(state)
		return err
	}
	if replacement == nil {
		c.finishReplacing(state)
		return errors.New("session factory returned nil session")
	}

	c.mu.Lock()
	if c.replace != state {
		closed := c.closed
		c.mu.Unlock()
		_ = replacement.Close()
		if closed {
			return ErrClosed
		}
		return ErrClosed
	}
	state.replacement = replacement
	state.phase = replacementPhaseBuilding
	c.mu.Unlock()

	runner := c.build(replacement)
	if runner == nil {
		c.mu.Lock()
		same := c.replace == state
		replacementClosed := state.replacementClosed
		closed := c.closed
		if same {
			c.finishReplacingLocked(state)
		}
		c.mu.Unlock()
		if !replacementClosed {
			_ = replacement.Close()
		}
		if closed {
			return ErrClosed
		}
		return errors.New("runner factory returned nil runner")
	}

	c.mu.Lock()
	if c.replace != state {
		closed := c.closed
		replacementClosed := state.replacementClosed
		c.mu.Unlock()
		if !replacementClosed {
			_ = replacement.Close()
		}
		if closed {
			return ErrClosed
		}
		return ErrClosed
	}
	state.phase = replacementPhaseClosingCurrent
	c.mu.Unlock()

	if err := current.Close(); err != nil {
		c.mu.Lock()
		same := c.replace == state
		replacementClosed := state.replacementClosed
		if same {
			c.finishClosedLocked(err)
			c.finishReplacingLocked(state)
		}
		c.mu.Unlock()
		if !replacementClosed {
			_ = replacement.Close()
		}
		return err
	}

	c.mu.Lock()
	if c.replace != state {
		closed := c.closed
		replacementClosed := state.replacementClosed
		c.mu.Unlock()
		if !replacementClosed {
			_ = replacement.Close()
		}
		if closed {
			return ErrClosed
		}
		return ErrClosed
	}
	c.current = replacement
	c.runner = runner
	closed := c.closed
	c.finishReplacingLocked(state)
	c.mu.Unlock()
	if closed {
		return ErrClosed
	}
	return nil
}

func (c *Controller) Info() Info {
	c.mu.Lock()
	current := c.current
	c.mu.Unlock()
	if current == nil {
		return Info{}
	}
	header := current.Header()
	return Info{
		SessionID:   header.ID,
		SessionPath: current.Path(),
		Workspace:   header.Workspace,
		Provider:    header.Provider,
		Profile:     header.Profile,
		Model:       header.Model,
	}
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
	c.mu.Lock()
	if c.closeDone != nil {
		done := c.closeDone
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

	if activeDone == nil && c.canFinishReplacementCloseLocked() {
		replace := c.replace
		current := c.current
		replacement := replace.replacement
		replace.replacementClosed = true
		c.finishReplacingLocked(replace)
		c.mu.Unlock()

		err := current.Close()
		if replacement != nil {
			if closeErr := replacement.Close(); err == nil {
				err = closeErr
			}
		}

		c.mu.Lock()
		if c.closeErr == nil {
			c.closeErr = err
		}
		err = c.closeErr
		close(done)
		c.mu.Unlock()
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
		close(done)
		c.mu.Unlock()
		return err
	}
	current := c.current
	c.mu.Unlock()

	var err error
	if current != nil {
		err = current.Close()
	}

	c.mu.Lock()
	if c.closeErr == nil {
		c.closeErr = err
	}
	err = c.closeErr
	close(done)
	c.mu.Unlock()
	return err
}

func (c *Controller) canFinishReplacementCloseLocked() bool {
	if c.replace == nil || c.replace.phase != replacementPhaseBuilding || c.replace.replacement == nil {
		return false
	}
	pcs := make([]uintptr, 32)
	n := runtime.Callers(3, pcs)
	frames := runtime.CallersFrames(pcs[:n])
	for {
		frame, more := frames.Next()
		if frame.Function == controllerNewSessionFunc {
			return true
		}
		if !more {
			return false
		}
	}
}

func (c *Controller) finishClosedLocked(err error) {
	c.closed = true
	c.current = nil
	c.runner = nil
	c.closeErr = err
	if c.closeDone == nil {
		c.closeDone = make(chan struct{})
		close(c.closeDone)
	}
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
