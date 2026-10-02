package agent

import (
	"context"
	"encoding/json"
	"errors"

	"github.com/coder/acp-go-sdk"
)

// ErrMemoryUnsupported is returned by the Memory methods when the agent does
// not advertise _meta.otto.memoryReview in its initialize response.
var ErrMemoryUnsupported = errors.New("agent does not support memory review")

// Errors the agent answers for _otto/memory/* with its own codes.
var (
	ErrMemoryUnavailable = errors.New("memory is not available in this session")
	ErrMemoryConflict    = errors.New("the candidate was already decided or changed")
	ErrCandidateNotFound = errors.New("candidate not found")
)

const (
	codeNotFound          = -32002
	codeMemoryUnavailable = -32010
	codeMemoryConflict    = -32011
)

// memoryErr turns the agent's _otto/memory/* error codes into the errors above.
func memoryErr(err error) error {
	var re *acp.RequestError
	if !errors.As(err, &re) {
		return err
	}
	switch re.Code {
	case codeMemoryUnavailable:
		return ErrMemoryUnavailable
	case codeMemoryConflict:
		return ErrMemoryConflict
	case codeNotFound:
		return ErrCandidateNotFound
	}
	return err
}

const (
	methodMemoryPending = "_otto/memory/pending"
	methodMemoryReview  = "_otto/memory/review"
)

// MemoryCandidate is one pending memory candidate.
type MemoryCandidate struct {
	ID     string `json:"id"`
	Action string `json:"action"`
	Kind   string `json:"kind"`
	Key    string `json:"key"`
	Text   string `json:"text"`
	Reason string `json:"reason"`
	Origin string `json:"origin"`
}

// MemoryReviewResult is the agent's answer to a review. Record is set when an
// accepted candidate wrote a record; Forgotten holds the id of a record an
// accepted forget candidate removed.
type MemoryReviewResult struct {
	Decision    string `json:"decision"`
	CandidateID string `json:"candidateId"`
	Record      *struct {
		ID       string `json:"id"`
		Revision uint64 `json:"revision"`
	} `json:"record"`
	Forgotten string `json:"forgotten"`
}

// memoryProcess returns the running process after checking that the agent
// advertises memory review. The session must be open (see Load).
func (a *Agent) memoryProcess(ctx context.Context, id string) (*process, error) {
	p, err := a.process(ctx)
	if err != nil {
		return nil, err
	}
	if !p.memoryReview {
		return nil, ErrMemoryUnsupported
	}
	if !p.isOpen(id) {
		return nil, ErrNotOpen
	}
	return p, nil
}

// MemoryPage is one page of pending candidates; NextCursor is empty on the
// last page.
type MemoryPage struct {
	Candidates []MemoryCandidate `json:"candidates"`
	NextCursor string            `json:"nextCursor"`
}

// MemoryPending returns one page of the pending memory candidates visible to
// session id. An empty cursor starts at the first page.
func (a *Agent) MemoryPending(ctx context.Context, id, cursor string) (MemoryPage, error) {
	var page MemoryPage
	p, err := a.memoryProcess(ctx, id)
	if err != nil {
		return page, err
	}
	raw, err := p.conn.CallExtension(ctx, methodMemoryPending, map[string]string{"sessionId": id, "cursor": cursor})
	if err != nil {
		return page, memoryErr(p.wrap(err))
	}
	return page, json.Unmarshal(raw, &page)
}

// MemoryReview accepts or rejects one candidate; decision is "accept" or
// "reject".
func (a *Agent) MemoryReview(ctx context.Context, id, candidateID, decision string) (MemoryReviewResult, error) {
	var out MemoryReviewResult
	p, err := a.memoryProcess(ctx, id)
	if err != nil {
		return out, err
	}
	raw, err := p.conn.CallExtension(ctx, methodMemoryReview, map[string]string{
		"sessionId": id, "candidateId": candidateID, "decision": decision,
	})
	if err != nil {
		return out, memoryErr(p.wrap(err))
	}
	return out, json.Unmarshal(raw, &out)
}
