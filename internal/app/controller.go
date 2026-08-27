package app

import (
	"context"
	"encoding/json"
	"errors"
	"sync"

	"github.com/baiyuqing/otto/internal/agent"
	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

var (
	ErrPromptActive = errors.New("prompt already active")
	ErrClosed       = errors.New("controller is closed")
)

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

type Controller struct {
	mu          sync.Mutex
	current     session.Session
	runner      Runner
	create      SessionFactory
	build       RunnerFactory
	prompting   bool
	replacing   bool
	closed      bool
	activeDone  chan struct{}
	replaceDone chan struct{}
	closeDone   chan struct{}
	closeErr    error
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
	if c.prompting || c.replacing {
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
	if c.prompting || c.replacing {
		c.mu.Unlock()
		return ErrPromptActive
	}
	done := make(chan struct{})
	c.replacing = true
	c.replaceDone = done
	current := c.current
	c.mu.Unlock()

	replacement, err := c.create()
	if err != nil {
		c.finishReplacing(done)
		return err
	}
	if replacement == nil {
		c.finishReplacing(done)
		return errors.New("session factory returned nil session")
	}

	runner := c.build(replacement)
	if runner == nil {
		_ = replacement.Close()
		c.finishReplacing(done)
		return errors.New("runner factory returned nil runner")
	}

	if err := current.Close(); err != nil {
		_ = replacement.Close()
		c.mu.Lock()
		c.finishClosedLocked(err)
		c.finishReplacingLocked(done)
		c.mu.Unlock()
		return err
	}

	c.mu.Lock()
	c.current = replacement
	c.runner = runner
	closed := c.closed
	c.finishReplacingLocked(done)
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
	replaceDone := c.replaceDone
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

func (c *Controller) finishReplacing(done chan struct{}) {
	c.mu.Lock()
	c.finishReplacingLocked(done)
	c.mu.Unlock()
}

func (c *Controller) finishReplacingLocked(done chan struct{}) {
	c.replacing = false
	if c.replaceDone == done {
		c.replaceDone = nil
		close(done)
	}
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
