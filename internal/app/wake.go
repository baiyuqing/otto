package app

import (
	"context"

	"github.com/baiyuqing/otto/internal/agent"
)

// WakeOperation is one admitted, empty-text notification turn.
type WakeOperation struct {
	controller *Controller
	active     *activeOperation
	claimCtx   context.Context
	stop       func() bool
}

// PrepareWake claims a turn only when a runner has pending task notifications.
// A caller must Run or Cancel the returned operation.
func (c *Controller) PrepareWake(ctx context.Context) (*WakeOperation, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return nil, ErrClosed
	}
	if c.active != nil || c.replace != nil {
		return nil, ErrPromptActive
	}
	owner, ok := c.runner.(taskOwner)
	if !ok || owner.Tasks() == nil || owner.Tasks().Pending() == 0 {
		return nil, nil
	}
	active := &activeOperation{runner: c.runner, current: c.current}
	wake := &WakeOperation{controller: c, active: active, claimCtx: ctx}
	active.wake = wake
	wake.stop = context.AfterFunc(ctx, wake.Cancel)
	c.active = active
	return wake, nil
}

// Run executes the claimed wake turn once.
func (w *WakeOperation) Run(ctx context.Context, emit func(agent.Event)) error {
	if w == nil || w.controller == nil || w.active == nil {
		return context.Canceled
	}
	if err := w.claimCtx.Err(); err != nil {
		w.Cancel()
		return err
	}
	if err := ctx.Err(); err != nil {
		w.Cancel()
		return err
	}
	c := w.controller
	c.mu.Lock()
	if w.active.released || c.active != w.active || w.active.started {
		closed := c.closed
		c.mu.Unlock()
		if closed {
			return ErrClosed
		}
		return context.Canceled
	}
	w.active.started = true
	stop := w.stop
	c.mu.Unlock()
	if stop != nil {
		stop()
	}
	defer c.endOperation(w.active)
	return w.active.runner.Run(ctx, "", emit)
}

// Cancel releases an unstarted wake claim. It is safe to call repeatedly.
func (w *WakeOperation) Cancel() {
	if w == nil || w.controller == nil || w.active == nil {
		return
	}
	c := w.controller
	c.mu.Lock()
	if c.active == w.active && !w.active.started {
		w.active.released = true
		c.active = nil
	}
	c.mu.Unlock()
	if w.stop != nil {
		w.stop()
	}
}
