// Package safetext provides deterministic text canonicalization for security
// boundaries shared by otherwise independent packages.
package safetext

import (
	"strings"
	"unicode/utf8"
)

// CanonicalizeUTF8 replaces each invalid UTF-8 byte with U+FFFD. Valid input
// is returned unchanged.
func CanonicalizeUTF8(value string) string {
	if value == "" || utf8.ValidString(value) {
		return value
	}

	var canonical strings.Builder
	canonical.Grow(len(value))
	for len(value) > 0 {
		r, size := utf8.DecodeRuneInString(value)
		if r == utf8.RuneError && size == 1 {
			canonical.WriteRune(utf8.RuneError)
			value = value[1:]
			continue
		}
		canonical.WriteString(value[:size])
		value = value[size:]
	}
	return canonical.String()
}
