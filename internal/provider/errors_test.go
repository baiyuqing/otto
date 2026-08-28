package provider

import (
	"errors"
	"strings"
	"testing"
)

func TestContextOverflowErrorPreservesIdentityAndUsesBoundedMetadata(t *testing.T) {
	overflow := &ContextOverflowError{
		Status:        400,
		Code:          "context_length_exceeded",
		CurrentTokens: 130000,
		MaximumTokens: 128000,
	}

	if !errors.Is(overflow, ErrContextOverflow) {
		t.Fatal("ContextOverflowError does not match ErrContextOverflow")
	}
	var typed *ContextOverflowError
	if !errors.As(overflow, &typed) || typed != overflow {
		t.Fatalf("errors.As() = %#v, want original error", typed)
	}
	message := overflow.Error()
	for _, want := range []string{"context window exceeded", "400", "context_length_exceeded", "130000", "128000"} {
		if !strings.Contains(message, want) {
			t.Fatalf("Error() = %q, want %q", message, want)
		}
	}
}

func TestContextOverflowErrorDoesNotMatchUnrelatedErrors(t *testing.T) {
	if errors.Is(&ContextOverflowError{}, errors.New("context window exceeded")) {
		t.Fatal("ContextOverflowError matched an unrelated error")
	}
}
