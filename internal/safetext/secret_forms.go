package safetext

import (
	"encoding/json"
	"strings"
)

// SecretForms returns at most two immutable, canonical forms of a secret: the
// supplied text and, when that text is valid JSON string content containing an
// escape, its JSON-decoded equivalent. The result count is deliberately
// bounded, and decoding cannot produce more bytes than the wrapped input.
func SecretForms(raw string) []string {
	canonical := CanonicalizeUTF8(raw)
	if canonical == "" {
		return nil
	}
	forms := []string{strings.Clone(canonical)}
	if !strings.ContainsRune(canonical, '\\') || len(canonical) > int(^uint(0)>>1)-2 {
		return forms
	}

	wrapped := make([]byte, len(canonical)+2)
	wrapped[0] = '"'
	copy(wrapped[1:], canonical)
	wrapped[len(wrapped)-1] = '"'
	var decoded string
	if err := json.Unmarshal(wrapped, &decoded); err != nil {
		return forms
	}
	decoded = CanonicalizeUTF8(decoded)
	if decoded == "" || decoded == canonical {
		return forms
	}
	return append(forms, strings.Clone(decoded))
}
