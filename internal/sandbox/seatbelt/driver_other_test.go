//go:build !darwin

package seatbelt

import (
	"context"
	"errors"
	"testing"

	"github.com/baiyuqing/otto/internal/sandbox"
)

func TestOpenReportsUnsupportedPlatformWithoutFallback(t *testing.T) {
	driver, err := Open(context.Background(), Options{
		Workspace: "/unsupported-workspace",
		Shell:     "/bin/sh",
		Home:      "/unsupported-home",
		Network:   sandbox.NetworkAllow,
	})
	if driver != nil {
		t.Fatal("Open() constructed a Driver on an unsupported platform")
	}
	var unavailable *sandbox.UnavailableError
	if !errors.Is(err, sandbox.ErrUnavailable) || !errors.As(err, &unavailable) ||
		unavailable.Reason != sandbox.ReasonUnsupportedPlatform ||
		err.Error() != (&sandbox.UnavailableError{Reason: sandbox.ReasonUnsupportedPlatform}).Error() {
		t.Fatalf("Open() error = %v, want typed unsupported-platform", err)
	}
}
