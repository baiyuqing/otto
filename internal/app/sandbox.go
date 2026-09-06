package app

import "context"

// SandboxReload re-reads the sandbox configuration and replaces the process
// sandbox, returning the state that is now in effect.
type SandboxReload func(context.Context) (SandboxInfo, error)

// SandboxReloader exposes the optional sandbox-reload capability. Frontends
// type-assert it on the Backend, matching how ProfileSwitcher is consumed.
type SandboxReloader interface {
	ReloadSandbox(context.Context) (SandboxInfo, error)
}

var _ SandboxReloader = (*Controller)(nil)

// WithSandboxControl wires the process sandbox. info reports the state that is
// currently in effect; the composition root owns a single sandbox, so every
// controller built from it reports the same live state rather than the value
// captured at startup.
func WithSandboxControl(info func() SandboxInfo, reload SandboxReload) Option {
	return func(controller *Controller) {
		controller.sandboxSource = info
		controller.reloadSandbox = reload
	}
}

// ReloadSandbox applies the current sandbox configuration to the running
// process. It is refused while a turn is active because a turn's bash tool may
// be executing a command through the sandbox being replaced.
func (c *Controller) ReloadSandbox(ctx context.Context) (SandboxInfo, error) {
	c.mu.Lock()
	reload := c.reloadSandbox
	switch {
	case c.closed:
		c.mu.Unlock()
		return SandboxInfo{}, ErrClosed
	case reload == nil:
		c.mu.Unlock()
		return SandboxInfo{}, ErrSandboxReloadUnavailable
	case c.active != nil || c.replace != nil:
		c.mu.Unlock()
		return SandboxInfo{}, ErrPromptActive
	}
	c.mu.Unlock()

	return reload(ctx)
}

// currentSandboxInfoLocked reports the live process sandbox when one is wired
// and otherwise the value captured at startup.
func (c *Controller) currentSandboxInfoLocked() SandboxInfo {
	if c.sandboxSource != nil {
		return c.sandboxSource()
	}
	return c.sandboxInfo
}
