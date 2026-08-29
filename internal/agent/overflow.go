package agent

import (
	"context"
	"errors"

	"github.com/baiyuqing/otto/internal/provider"
)

const (
	automaticCompactionWarningMessage       = "automatic context compaction failed below the hard input limit; continuing with the original request"
	automaticCompactionHardFailureMessage   = "automatic context compaction failed at the hard input limit"
	automaticCompactionStillTooLargeMessage = "compacted context still exceeds the hard input limit"
	automaticCompactionAttemptUsedMessage   = "context exceeds the hard input limit after the automatic compaction attempt"
	overflowCompactionFailureMessage        = "context overflow recovery could not compact the current request"
	overflowRetryFailureMessage             = "context overflow persisted after one automatic compaction retry"
)

var errAutomaticCompactionWarning = errors.New(automaticCompactionWarningMessage)

type runDispatchState struct {
	proactiveAttempted bool
}

type automaticDispatchError struct {
	message string
	causes  []error
}

func (e *automaticDispatchError) Error() string { return e.message }

func (e *automaticDispatchError) Unwrap() []error {
	return append([]error(nil), e.causes...)
}

func newAutomaticDispatchError(message string, causes ...error) error {
	boundedCauses := make([]error, 0, len(causes))
	for _, cause := range causes {
		if cause != nil {
			boundedCauses = append(boundedCauses, cause)
		}
	}
	return &automaticDispatchError{message: message, causes: boundedCauses}
}

func automaticCancellation(ctx context.Context, err error) error {
	if ctxErr := ctx.Err(); ctxErr != nil {
		return ctxErr
	}
	if errors.Is(err, context.Canceled) || errors.Is(err, context.DeadlineExceeded) {
		return err
	}
	return nil
}

func automaticCompactionTriggers(settings CompactionSettings) (soft int, hard int, known bool) {
	if settings.WorkingWindow <= 0 || settings.HardInputWindow <= 0 {
		return 0, 0, false
	}
	reserve := max(settings.ReserveTokens, 0)
	return settings.WorkingWindow - reserve, settings.HardInputWindow - reserve, true
}

func isTypedContextOverflow(err error) bool {
	return errors.Is(err, provider.ErrContextOverflow)
}
