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

func TestRedactorRedactsJSONKeysWithoutLosingNumberFidelity(t *testing.T) {
	const credential = "resolved-secret"
	redactor := NewRedactor([]string{credential})
	raw := json.RawMessage(`{"resolved-secret":9007199254740993123456789,"nested":{"prefix-resolved-secret-suffix":1.2300e+45}}`)

	got := redactor.RedactJSONStrings(raw)
	if !json.Valid(got) || !utf8.Valid(got) {
		t.Fatalf("RedactJSONStrings() returned invalid JSON/UTF-8: %q", got)
	}
	if strings.Contains(string(got), credential) {
		t.Fatalf("RedactJSONStrings() leaked credential-bearing key: %s", got)
	}
	for _, number := range []string{"9007199254740993123456789", "1.2300e+45"} {
		if !strings.Contains(string(got), number) {
			t.Fatalf("RedactJSONStrings() lost number %q fidelity: %s", number, got)
		}
	}
}

func TestRedactorJSONKeyRedactionIsSafeAndDeterministic(t *testing.T) {
	for _, test := range []struct {
		name        string
		credentials []string
		raw         json.RawMessage
	}{
		{
			name:        "credential equals default marker",
			credentials: []string{redactionMarker},
			raw:         json.RawMessage(`{"█":"first","":"second"}`),
		},
		{
			name:        "overlapping credentials",
			credentials: []string{"secret", "resolved-secret"},
			raw:         json.RawMessage(`{"resolved-secret":{"secret":"value"}}`),
		},
		{
			name:        "root and nested collisions",
			credentials: []string{"secret"},
			raw:         json.RawMessage(`{"secret":"first","█":"second","nested":{"secret":1,"█":2}}`),
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			redactor := NewRedactor(test.credentials)
			var first string
			for iteration := 0; iteration < 100; iteration++ {
				got := redactor.RedactJSONStrings(test.raw)
				if !json.Valid(got) || !utf8.Valid(got) {
					t.Fatalf("iteration %d returned invalid JSON/UTF-8: %q", iteration, got)
				}
				for _, credential := range test.credentials {
					if strings.Contains(string(got), credential) {
						t.Fatalf("iteration %d leaked credential %q: %s", iteration, credential, got)
					}
				}
				if iteration == 0 {
					first = string(got)
				} else if string(got) != first {
					t.Fatalf("nondeterministic output: first %q, iteration %d %q", first, iteration, got)
				}
			}
			if test.name == "root and nested collisions" && first != `{"nested":{"█":null},"█":null}` {
				t.Fatalf("collision output = %s, want deterministic semantic loss", first)
			}
		})
	}
}

func TestRedactorJSONKeyRedactionReturnsValidUTF8ForInvalidKeyBytes(t *testing.T) {
	const credential = "secret"
	raw := json.RawMessage(append([]byte(`{"secret-`), 0xff, '"', ':', '1', '}'))
	got := NewRedactor([]string{credential}).RedactJSONStrings(raw)
	if !json.Valid(got) || !utf8.Valid(got) || strings.Contains(string(got), credential) {
		t.Fatalf("RedactJSONStrings() = %q, want valid credential-free UTF-8 JSON", got)
	}
}

func TestRedactorOverlappingJSONKeySecretsDoNotDependOnConfigurationOrder(t *testing.T) {
	raw := json.RawMessage(`{"abc":"value"}`)
	forward := NewRedactor([]string{"ab", "bc"}).RedactJSONStrings(raw)
	reverse := NewRedactor([]string{"bc", "ab"}).RedactJSONStrings(raw)
	if string(forward) != string(reverse) {
		t.Fatalf("overlapping key output depends on credential order: %s != %s", forward, reverse)
	}
}
