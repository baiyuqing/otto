package safetext

import (
	"testing"
	"unicode/utf8"
)

func TestCanonicalizeUTF8MatchesReplacementRuneSemantics(t *testing.T) {
	valid := "already valid €"
	if got := CanonicalizeUTF8(valid); got != valid {
		t.Fatalf("CanonicalizeUTF8(valid) = %q, want %q", got, valid)
	}

	invalid := string([]byte{'a', 0xff, 0xc0, 0xaf, 'b'})
	got := CanonicalizeUTF8(invalid)
	if !utf8.ValidString(got) || got != "a���b" {
		t.Fatalf("CanonicalizeUTF8(invalid) = %q", got)
	}
}
