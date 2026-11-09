package bridge

// chatActivity tracks queue-worker or session-switch ownership. It does not
// track approvals, task-control workers, session routing or reply barriers.
type chatActivity uint8

const (
	chatIdle chatActivity = iota
	chatSwitching
	chatWorking
	chatStopping
)

// All activity queries and transitions require c.mu.
func (c *chat) workerOwnedLocked() bool {
	return c.activity == chatWorking || c.activity == chatStopping
}

func (c *chat) switchingLocked() bool {
	return c.activity == chatSwitching
}

func (c *chat) stopRequestedLocked() bool {
	return c.activity == chatStopping
}

func (c *chat) beginWorkerLocked() bool {
	if c.activity != chatIdle {
		return false
	}
	c.activity = chatWorking
	return true
}

// beginMessageLocked starts a dequeued message with fresh stop intent. The
// worker retains ownership across the preceding turn's final reply.
func (c *chat) beginMessageLocked() {
	if c.workerOwnedLocked() {
		c.activity = chatWorking
	}
}

func (c *chat) finishWorkerLocked() {
	if c.workerOwnedLocked() {
		c.activity = chatIdle
	}
}

func (c *chat) beginSwitchLocked() bool {
	if c.activity != chatIdle {
		return false
	}
	c.activity = chatSwitching
	return true
}

func (c *chat) finishSwitchLocked() {
	if c.switchingLocked() {
		c.activity = chatIdle
	}
}

// requestStopLocked retains worker ownership until its queue drain finishes.
// An approval-only stop cannot claim a worker or release a session switch.
func (c *chat) requestStopLocked() bool {
	if !c.workerOwnedLocked() {
		return false
	}
	c.activity = chatStopping
	return true
}
