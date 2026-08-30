package memory

import (
	"context"
	"errors"
	"testing"
)

func TestDefaultPolicyAuthorityMatrix(t *testing.T) {
	tests := []struct {
		origin           Origin
		sensitive, valid bool
		want             PolicyDecision
	}{
		{OriginHuman, false, true, PolicyAccept},
		{OriginModel, false, true, PolicyPending},
		{OriginExtractor, false, true, PolicyPending},
		{OriginImport, false, true, PolicyPending},
		{OriginMigration, false, true, PolicyAccept},
		{OriginHuman, true, true, PolicyReject},
		{OriginHuman, false, false, PolicyReject},
		{Origin("unknown"), false, true, PolicyReject},
	}
	for _, tt := range tests {
		got, err := (DefaultPolicy{}).Decide(context.Background(), PolicyRequest{
			Origin: tt.origin, Sensitive: tt.sensitive, Valid: tt.valid,
		})
		if err != nil {
			t.Fatalf("origin %q: %v", tt.origin, err)
		}
		if got != tt.want {
			t.Fatalf("origin %q: decision = %q, want %q", tt.origin, got, tt.want)
		}
	}
}

func TestDefaultPolicyChecksContextFirst(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	got, err := (DefaultPolicy{}).Decide(ctx, PolicyRequest{
		Origin: Origin("unknown"), Valid: false, Sensitive: true,
	})
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("error = %v, want context.Canceled", err)
	}
	if got != "" {
		t.Fatalf("decision = %q, want empty decision", got)
	}
}

func TestDefaultPolicyRejectsMismatchedProvenanceAuthority(t *testing.T) {
	got, err := (DefaultPolicy{}).Decide(context.Background(), PolicyRequest{
		Origin: OriginHuman,
		Source: Provenance{Origin: OriginModel},
		Valid:  true,
	})
	if err != nil {
		t.Fatal(err)
	}
	if got != PolicyReject {
		t.Fatalf("decision = %q, want %q", got, PolicyReject)
	}
}
