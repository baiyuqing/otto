package memory

import "context"

// NoopExtractor never proposes candidates. It satisfies Extractor until a
// real extraction implementation is wired (design spec Phase 4).
type NoopExtractor struct{}

func (NoopExtractor) Extract(context.Context, ExtractRequest) ([]Proposal, error) {
	return nil, nil
}
