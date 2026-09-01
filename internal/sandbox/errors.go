package sandbox

import "errors"

var (
	ErrClosed            = errors.New("sandbox executor is closed")
	ErrInvalidRequest    = errors.New("invalid sandbox request")
	ErrUnsupportedPolicy = errors.New("sandbox policy is unsupported")
	ErrUnavailable       = errors.New("sandbox driver is unavailable")
	ErrEnvironmentUnsafe = errors.New("sandbox environment is unsafe")
	ErrChildLaunch       = errors.New("sandbox child launch failed")
	ErrChildWait         = errors.New("sandbox child wait failed")
	ErrChildTerminate    = errors.New("sandbox child termination failed")
)

type UnavailableReason string

const (
	ReasonUnsupportedPlatform UnavailableReason = "unsupported-platform"
	ReasonSeatbeltMissing     UnavailableReason = "seatbelt-missing"
	ReasonSelfTestFailed      UnavailableReason = "self-test-failed"
	ReasonRuntimeFailure      UnavailableReason = "runtime-failure"
	ReasonInvalidShell        UnavailableReason = "invalid-shell"
	ReasonEnvironmentRejected UnavailableReason = "environment-rejected"
	ReasonPolicyUnsupported   UnavailableReason = "policy-unsupported"
)

type UnavailableError struct {
	Reason UnavailableReason
}

func (e *UnavailableError) Error() string {
	reason := e.Reason
	if !validUnavailableReason(reason) {
		reason = ReasonRuntimeFailure
	}
	return ErrUnavailable.Error() + ": " + string(reason)
}

func (e *UnavailableError) Is(target error) bool {
	return target == ErrUnavailable
}

func validUnavailableReason(reason UnavailableReason) bool {
	switch reason {
	case ReasonUnsupportedPlatform,
		ReasonSeatbeltMissing,
		ReasonSelfTestFailed,
		ReasonRuntimeFailure,
		ReasonInvalidShell,
		ReasonEnvironmentRejected,
		ReasonPolicyUnsupported:
		return true
	default:
		return false
	}
}
