package safetext

import (
	"encoding/json"
	"slices"
	"strings"
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

func TestDynamicRedactionMarkerAvoidsNestedJSONEscapeSynthesis(t *testing.T) {
	forms := SecretForms(`\\u003c`)
	marker, ok := DynamicRedactionMarker(forms)
	if !ok || marker == "" {
		t.Fatal("DynamicRedactionMarker() did not find a safe shared marker")
	}
	encoded, err := json.Marshal(marker)
	if err != nil {
		t.Fatal(err)
	}
	for _, form := range forms {
		if strings.Contains(marker, form) || strings.Contains(string(encoded[1:len(encoded)-1]), form) {
			t.Fatalf("marker %q or encoding %s synthesized retained form %q", marker, encoded, form)
		}
	}
}

func TestDynamicRedactionMarkerRejectsOversizedCapabilitySets(t *testing.T) {
	if marker, ok := DynamicRedactionMarker([]string{strings.Repeat("a", 257)}); ok || marker != "" {
		t.Fatalf("oversized form marker = %q ok=%t, want suppression", marker, ok)
	}
	forms := make([]string, 65)
	for i := range forms {
		forms[i] = strings.Repeat("a", i) + "z"
	}
	if marker, ok := DynamicRedactionMarker(forms); ok || marker != "" {
		t.Fatalf("65-form marker = %q ok=%t, want suppression", marker, ok)
	}
}
