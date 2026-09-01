//go:build darwin

package nativeprocess

import (
	"errors"
	"fmt"
	"time"

	"golang.org/x/sys/unix"
)

type processExitObserver struct {
	pid    int
	kqueue int
}

func newProcessExitObserver(pid int) (*processExitObserver, error) {
	queue, err := unix.Kqueue()
	if err != nil {
		return nil, err
	}
	change := unix.Kevent_t{
		Ident:  uint64(pid),
		Filter: unix.EVFILT_PROC,
		Flags:  unix.EV_ADD | unix.EV_ONESHOT,
		Fflags: unix.NOTE_EXIT,
	}
	if _, err := unix.Kevent(queue, []unix.Kevent_t{change}, nil, nil); err != nil {
		_ = unix.Close(queue)
		return nil, err
	}
	return &processExitObserver{pid: pid, kqueue: queue}, nil
}

func (o *processExitObserver) Wait(timeout time.Duration) error {
	if o == nil || o.kqueue < 0 || timeout <= 0 {
		return errors.New("invalid process exit observer")
	}
	events := make([]unix.Kevent_t, 1)
	deadline := unix.NsecToTimespec(timeout.Nanoseconds())
	count, err := unix.Kevent(o.kqueue, nil, events, &deadline)
	if err != nil {
		return err
	}
	if count != 1 {
		return errors.New("process exit event deadline exceeded")
	}
	event := events[0]
	if event.Ident != uint64(o.pid) || event.Filter != unix.EVFILT_PROC || event.Fflags&unix.NOTE_EXIT == 0 {
		return fmt.Errorf("unexpected process event: ident=%d filter=%d flags=%#x", event.Ident, event.Filter, event.Fflags)
	}
	return nil
}

func (o *processExitObserver) Close() error {
	if o == nil || o.kqueue < 0 {
		return nil
	}
	queue := o.kqueue
	o.kqueue = -1
	return unix.Close(queue)
}
