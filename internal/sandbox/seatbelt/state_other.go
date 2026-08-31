//go:build !darwin && !linux

package seatbelt

import (
	"errors"
	"sync"

	"github.com/baiyuqing/otto/internal/sandbox"
)

var errStateCreate = errors.New("seatbelt private state unavailable")

type state struct {
	directories sandbox.PrivateDirectories
	profiles    string
	profilePath string
	rootParent  string
	closeOnce   sync.Once
	closeErr    error
}

func createState(_, _ string) (*state, error) {
	return nil, errStateCreate
}

func (s *state) writeProfile(_ []byte) error {
	return errStateCreate
}

func (s *state) close() error {
	return nil
}
