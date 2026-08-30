package memory

import (
	"errors"
	"fmt"
)

var (
	ErrDisabled            = errors.New("memory is disabled")
	ErrUnavailable         = errors.New("memory is unavailable")
	ErrConflict            = errors.New("memory revision conflict")
	ErrSensitiveMemory     = errors.New("memory contains sensitive data")
	ErrUnsupported         = errors.New("memory operation is unsupported")
	ErrMemoryInUse         = errors.New("memory is in use")
	ErrPersistenceDisabled = errors.New("memory persistence is disabled")
	ErrBusy                = errors.New("memory store is busy")
	ErrCommitUnknown       = errors.New("memory commit outcome is unknown")
	ErrCorrupt             = errors.New("memory data is corrupt")
	ErrIncompatibleSchema  = errors.New("memory schema is incompatible")
	ErrInvalidRecord       = errors.New("invalid memory record")
	ErrInvalidRequest      = errors.New("invalid memory request")
	ErrNotFound            = errors.New("memory entity not found")
	ErrClosed              = errors.New("memory is closed")
	ErrInvalidCursor       = errors.New("invalid memory cursor")
	ErrIncompleteForget    = errors.New("memory was forgotten but tombstone recording is incomplete")
	ErrIncompletePurge     = errors.New("memory backup purge is incomplete")
)

type ConflictError struct {
	EntityKind       string
	ID               string
	ExpectedRevision uint64
	ActualRevision   uint64
}

func (*ConflictError) Error() string {
	return ErrConflict.Error()
}

func (*ConflictError) Unwrap() error {
	return ErrConflict
}

type CommitUnknownError struct {
	operation CommitOperation
	entityIDs []string
}

func NewCommitUnknownError(operation CommitOperation, entityIDs []string) (*CommitUnknownError, error) {
	if !validCommitOperation(operation) {
		return nil, fmt.Errorf("%w: commit operation", ErrInvalidRequest)
	}
	if len(entityIDs) > MaxCommitUnknownIDs {
		return nil, fmt.Errorf("%w: commit entity ID count exceeds %d", ErrInvalidRequest, MaxCommitUnknownIDs)
	}
	for _, id := range entityIDs {
		if !validOpaqueID(id, MaxIDBytes) {
			return nil, fmt.Errorf("%w: commit entity ID", ErrInvalidRequest)
		}
	}
	return &CommitUnknownError{
		operation: operation,
		entityIDs: cloneStrings(entityIDs),
	}, nil
}

func (*CommitUnknownError) Error() string {
	return "memory commit outcome is unknown; reconcile through Reader.Get or Store.ListCandidates before retry"
}

func (*CommitUnknownError) Unwrap() error {
	return ErrCommitUnknown
}

func (e *CommitUnknownError) Operation() CommitOperation {
	if e == nil {
		return ""
	}
	return e.operation
}

func (e *CommitUnknownError) EntityIDs() []string {
	if e == nil {
		return nil
	}
	return cloneStrings(e.entityIDs)
}

func validCommitOperation(operation CommitOperation) bool {
	switch operation {
	case CommitSchema, CommitUpsert, CommitForget, CommitPropose, CommitObserve, CommitReview:
		return true
	default:
		return false
	}
}

func validOpaqueID(id string, maxBytes int) bool {
	if len(id) == 0 || len(id) > maxBytes {
		return false
	}
	for i := 0; i < len(id); i++ {
		c := id[i]
		if (c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') {
			continue
		}
		switch c {
		case '.', '_', ':', '-':
			continue
		default:
			return false
		}
	}
	return true
}
