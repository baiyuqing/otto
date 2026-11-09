package bridge

import "context"

type taskControlPhase uint8

const (
	taskControlIdle taskControlPhase = iota
	taskControlActive
	taskControlDraining
)

// A task binds its cancellation, queued controls and workers to one generation.
// Identity/context fields are immutable; other fields require chat.mu.
type controlTask struct {
	generation uint64
	sessionID  string
	ctx        context.Context
	cancel     context.CancelFunc
	phase      taskControlPhase
	controls   []controlItem
	worker     *controlWorker
}

// The worker retains its task even after /stop detaches it from the chat.
type controlWorker struct {
	task *controlTask
	done chan struct{}
}

// All queries/transitions below require c.mu.
func (c *chat) taskControlPhaseLocked() taskControlPhase {
	if c.controlTask == nil {
		return taskControlIdle
	}
	return c.controlTask.phase
}

func (c *chat) taskControlAvailableLocked() bool {
	return c.controlTask != nil && c.controlTask.sessionID != "" &&
		c.taskControlCurrentLocked(c.controlTask)
}

func (c *chat) taskControlCurrentLocked(task *controlTask) bool {
	return task != nil && c.controlTask == task && c.taskGeneration == task.generation && task.phase != taskControlIdle
}

func (c *chat) beginTaskControlLocked(parent context.Context, sessionID string) *controlTask {
	c.taskGeneration++
	ctx, cancel := context.WithCancel(parent)
	task := &controlTask{
		generation: c.taskGeneration,
		sessionID:  sessionID,
		ctx:        ctx,
		cancel:     cancel,
		phase:      taskControlActive,
	}
	c.controlTask = task
	return task
}

func (c *chat) stopTaskControlLocked() {
	c.taskGeneration++
	if task := c.controlTask; task != nil {
		task.phase = taskControlIdle
		task.controls = nil
		task.cancel()
		c.controlTask = nil
	}
}

func (c *chat) controlWorkerCurrentLocked(worker *controlWorker) bool {
	return c.taskControlCurrentLocked(worker.task) && worker.task.worker == worker && worker.task.ctx.Err() == nil
}

func (c *chat) finishTaskControlWorkerLocked(worker *controlWorker) {
	if worker.task.worker == worker {
		worker.task.worker = nil
	}
	select {
	case <-worker.done:
	default:
		close(worker.done)
	}
}

func (c *chat) releaseTaskControlLocked(task *controlTask) {
	task.phase = taskControlIdle
	task.controls = nil
	task.cancel()
	if c.controlTask == task {
		c.controlTask = nil
		c.taskGeneration++
	}
}

// Main-task completion drains control replies before releasing the task.
// A stale completion waits only for the worker belonging to its captured task.
func (c *chat) finishTaskControl(task *controlTask) {
	for {
		c.mu.Lock()
		current := c.taskControlCurrentLocked(task)
		if current {
			task.phase = taskControlDraining
		}
		worker := task.worker
		if worker == nil {
			c.releaseTaskControlLocked(task)
			c.mu.Unlock()
			return
		}
		c.mu.Unlock()
		<-worker.done
		// A draining task may admit another worker before this finisher
		// reacquires the mutex after the previous worker's reply delivery.
	}
}
