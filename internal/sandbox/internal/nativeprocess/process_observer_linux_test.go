//go:build linux

package nativeprocess

import (
	"errors"
	"fmt"
	"time"

	"golang.org/x/sys/unix"
)

type processExitObserver struct {
	pidfd int
}

func newProcessExitObserver(pid int) (*processExitObserver, error) {
	fd, err := unix.PidfdOpen(pid, 0)
	if err != nil {
		return nil, err
	}
	return &processExitObserver{pidfd: fd}, nil
}

func (o *processExitObserver) Wait(timeout time.Duration) error {
	if o == nil || o.pidfd < 0 || timeout <= 0 {
		return errors.New("invalid process exit observer")
	}
	milliseconds := timeout.Milliseconds()
	if milliseconds <= 0 {
		milliseconds = 1
	}
	if milliseconds > int64(^uint(0)>>1) {
		milliseconds = int64(^uint(0) >> 1)
	}
	fds := []unix.PollFd{{Fd: int32(o.pidfd), Events: unix.POLLIN}}
	count, err := unix.Poll(fds, int(milliseconds))
	if err != nil {
		return err
	}
	if count != 1 {
		return errors.New("process exit event deadline exceeded")
	}
	if fds[0].Revents&(unix.POLLIN|unix.POLLHUP) == 0 {
		return fmt.Errorf("unexpected pidfd event flags: %#x", fds[0].Revents)
	}
	return nil
}

func (o *processExitObserver) Close() error {
	if o == nil || o.pidfd < 0 {
		return nil
	}
	fd := o.pidfd
	o.pidfd = -1
	return unix.Close(fd)
}
