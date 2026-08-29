package provider

import (
	"errors"
	"fmt"
	"strings"
)

var ErrContextOverflow = errors.New("context window exceeded")

type ContextOverflowError struct {
	Status        int
	Code          string
	CurrentTokens int
	MaximumTokens int
}

func (e *ContextOverflowError) Error() string {
	if e == nil {
		return ErrContextOverflow.Error()
	}
	details := make([]string, 0, 3)
	if e.Status > 0 {
		details = append(details, fmt.Sprintf("HTTP %d", e.Status))
	}
	if e.Code != "" {
		details = append(details, "code "+e.Code)
	}
	if e.CurrentTokens > 0 && e.MaximumTokens > 0 {
		details = append(details, fmt.Sprintf("requested %d tokens, maximum %d", e.CurrentTokens, e.MaximumTokens))
	}
	if len(details) == 0 {
		return ErrContextOverflow.Error()
	}
	return ErrContextOverflow.Error() + " (" + strings.Join(details, ", ") + ")"
}

func (e *ContextOverflowError) Is(target error) bool {
	return target == ErrContextOverflow
}
