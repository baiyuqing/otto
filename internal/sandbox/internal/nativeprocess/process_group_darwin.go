//go:build darwin

package nativeprocess

import (
	"time"

	"golang.org/x/sys/unix"
)

const (
	processGroupObservationDeadline = 100 * time.Millisecond
	darwinZombieProcessState        = 5 // SZOMB from sys/proc.h.
)

func processGroupTerminated(pgid int) bool {
	deadline := time.Now().Add(processGroupObservationDeadline)
	for {
		processes, err := unix.SysctlKinfoProcSlice("kern.proc.pgrp", pgid)
		if err != nil {
			return false
		}
		live := false
		for _, process := range processes {
			if process.Proc.P_stat != darwinZombieProcessState {
				live = true
				break
			}
		}
		if !live {
			return true
		}
		if time.Now().After(deadline) {
			return false
		}
		time.Sleep(time.Millisecond)
	}
}
