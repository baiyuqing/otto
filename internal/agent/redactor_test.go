package agent

import (
	"encoding/json"
	"strings"
	"testing"
	"unicode/utf8"
)

func TestRedactorNeverUsesAReplacementThatContainsTheCredential(t *testing.T) {
	for _, credential := range []string{"[REDACTED]", "REDACTED", "[", "界"} {
		t.Run(credential, func(t *testing.T) {
			redactor := NewRedactor([]string{credential})
			got := redactor.RedactString("before " + credential + " after")
			if strings.Contains(got, credential) {
				t.Fatalf("RedactString() = %q, still contains credential %q", got, credential)
			}
			if !utf8.ValidString(got) {
				t.Fatalf("RedactString() returned invalid UTF-8: %q", got)
			}
		})
	}
}

func TestRedactorRedactsRootAndNestedJSONStrings(t *testing.T) {
	const credential = "json-root-secret"
	redactor := NewRedactor([]string{credential})
	for _, raw := range []json.RawMessage{
		json.RawMessage(`"json-root-secret"`),
		json.RawMessage(`{"nested":["json-root-secret"]}`),
	} {
		got := redactor.RedactJSONStrings(raw)
		if !json.Valid(got) || strings.Contains(string(got), credential) {
			t.Fatalf("RedactJSONStrings(%s) = %s", raw, got)
		}
	}
}
