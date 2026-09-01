package safetext

import (
	"slices"
	"testing"
	"unicode/utf8"
)

func TestSecretFormsAddsOnlyJSONStringEscapeEquivalentDecodedForm(t *testing.T) {
	tests := []struct {
		raw  string
		want []string
	}{
		{raw: `less\u003cthan`, want: []string{`less\u003cthan`, "less<than"}},
		{raw: `upper\u003Ccase`, want: []string{`upper\u003Ccase`, "upper<case"}},
		{raw: `quote\"value`, want: []string{`quote\"value`, `quote"value`}},
		{raw: `back\\slash`, want: []string{`back\\slash`, `back\slash`}},
		{raw: `slash\/value`, want: []string{`slash\/value`, `slash/value`}},
		{raw: `line\nbreak`, want: []string{`line\nbreak`, "line\nbreak"}},
		{raw: `separator\u2028value`, want: []string{`separator\u2028value`, "separator\u2028value"}},
		{raw: `paragraph\u2029value`, want: []string{`paragraph\u2029value`, "paragraph\u2029value"}},
		{raw: `plain-value`, want: []string{`plain-value`}},
		{raw: `invalid\x3cvalue`, want: []string{`invalid\x3cvalue`}},
		{raw: `invalid\uZZZZvalue`, want: []string{`invalid\uZZZZvalue`}},
		{raw: `bare"quote`, want: []string{`bare"quote`}},
	}
	for _, test := range tests {
		t.Run(test.raw, func(t *testing.T) {
			if got := SecretForms(test.raw); !slices.Equal(got, test.want) {
				t.Fatalf("SecretForms(%q) = %#v, want %#v", test.raw, got, test.want)
			}
		})
	}
}

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
