package memory

import "context"

// DefaultPolicy applies the conservative built-in authority rules.
type DefaultPolicy struct{}

func (DefaultPolicy) Decide(ctx context.Context, request PolicyRequest) (PolicyDecision, error) {
	if err := ctx.Err(); err != nil {
		return "", err
	}
	if !request.Valid || request.Sensitive || request.Source.Origin != "" && request.Source.Origin != request.Origin {
		return PolicyReject, nil
	}

	switch request.Origin {
	case OriginHuman, OriginMigration:
		return PolicyAccept, nil
	case OriginModel, OriginExtractor, OriginImport:
		return PolicyPending, nil
	default:
		return PolicyReject, nil
	}
}
