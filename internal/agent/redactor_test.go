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

func TestRedactorReplacementCannotSynthesizeAnotherCredential(t *testing.T) {
	const source = "source-secret"
	const synthesized = "a["
	redactor := NewRedactor([]string{source, synthesized})

	got := redactor.RedactString("a" + source)
	for _, credential := range []string{source, synthesized} {
		if strings.Contains(got, credential) {
			t.Fatalf("RedactString() = %q, synthesized credential %q", got, credential)
		}
	}

	stream := redactor.newStream()
	var streamed strings.Builder
	streamed.WriteString(stream.Write("a"))
	for _, character := range source {
		streamed.WriteString(stream.Write(string(character)))
	}
	streamed.WriteString(stream.Flush())
	for _, credential := range []string{source, synthesized} {
		if strings.Contains(streamed.String(), credential) {
			t.Fatalf("stream output = %q, synthesized credential %q", streamed.String(), credential)
		}
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
