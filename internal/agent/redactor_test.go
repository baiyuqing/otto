package agent

import (
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"testing"
	"unicode/utf8"
)

func TestRedactorNormalizesInvalidUTF8BeforeExactReplacement(t *testing.T) {
	redactor := NewRedactor([]string{"prefix�"})
	invalid := string(append([]byte("prefix"), 0xff))
	got := redactor.RedactString(invalid)
	if !utf8.ValidString(got) {
		t.Fatalf("RedactString() returned invalid UTF-8: %q", got)
	}
	if strings.Contains(got, "prefix�") {
		t.Fatalf("RedactString() synthesized a configured secret: %q", got)
	}

	stream := redactor.newStream()
	streamed := stream.Write(invalid[:3]) + stream.Write(invalid[3:]) + stream.Flush()
	if !utf8.ValidString(streamed) || strings.Contains(streamed, "prefix�") {
		t.Fatalf("stream redaction normalized unsafely: %q", streamed)
	}
}

func TestRedactorPreservesInvalidCompactionSummaryIdentity(t *testing.T) {
	err := fmt.Errorf("%w: response contains secret", ErrInvalidCompactionSummary)
	got := NewRedactor([]string{"secret"}).RedactError(err)
	if !errors.Is(got, ErrInvalidCompactionSummary) || strings.Contains(got.Error(), "secret") {
		t.Fatalf("RedactError() = %v", got)
	}
}

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

func TestRedactorJSONDuplicateAndNormalizedKeysFailClosed(t *testing.T) {
	invalidUTF8 := append([]byte(`{"secret-`), 0xff)
	invalidUTF8 = append(invalidUTF8, []byte(`":"first","secret-`)...)
	invalidUTF8 = append(invalidUTF8, 0xfe)
	invalidUTF8 = append(invalidUTF8, []byte(`":"attacker"}`)...)

	for _, test := range []struct {
		name string
		raw  json.RawMessage
		want string
	}{
		{
			name: "exact duplicate",
			raw:  json.RawMessage(`{"safe":"first","safe":"attacker"}`),
			want: `{"safe":null}`,
		},
		{
			name: "escape alias",
			raw:  json.RawMessage(`{"a":"first","\u0061":"attacker"}`),
			want: `{"a":null}`,
		},
		{
			name: "unpaired surrogate variants",
			raw:  json.RawMessage(`{"secret-\ud800":"first","secret-\ud801":"attacker"}`),
			want: `{"█-�":null}`,
		},
		{
			name: "invalid UTF-8 variants",
			raw:  json.RawMessage(invalidUTF8),
			want: `{"█-�":null}`,
		},
		{
			name: "nested array and post-redaction collision",
			raw:  json.RawMessage(`{"items":[{"secret":"first","█":"attacker"},{"nested":{"a":"first","\u0061":"attacker"}}]}`),
			want: `{"items":[{"█":null},{"nested":{"a":null}}]}`,
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			got := NewRedactor([]string{"secret"}).RedactJSONStrings(test.raw)
			if !json.Valid(got) || !utf8.Valid(got) {
				t.Fatalf("RedactJSONStrings() returned invalid JSON/UTF-8: %q", got)
			}
			if string(got) != test.want {
				t.Fatalf("RedactJSONStrings() = %s, want %s", got, test.want)
			}
			if strings.Contains(string(got), "attacker") || strings.Contains(string(got), "secret") {
				t.Fatalf("RedactJSONStrings() retained colliding semantics or credential: %s", got)
			}
		})
	}
}

func TestRedactorJSONInvalidInputAndDepthFailClosed(t *testing.T) {
	redactor := NewRedactor([]string{"secret"})
	tooDeep := strings.Repeat("[", 10_001) + `"secret"` + strings.Repeat("]", 10_001)
	for _, raw := range []json.RawMessage{
		json.RawMessage(`{"secret":`),
		json.RawMessage([]byte{0xff}),
		json.RawMessage(tooDeep),
	} {
		got := redactor.RedactJSONStrings(raw)
		if string(got) != "null" || !json.Valid(got) || !utf8.Valid(got) {
			t.Fatalf("RedactJSONStrings() = %q, want valid UTF-8 fail-closed JSON", got)
		}
	}
}

func TestRedactorJSONMaximumSupportedDepthDoesNotPanic(t *testing.T) {
	const depth = 10_000
	raw := json.RawMessage(strings.Repeat("[", depth) + `"secret"` + strings.Repeat("]", depth))
	got := NewRedactor([]string{"secret"}).RedactJSONStrings(raw)
	if !json.Valid(got) || !utf8.Valid(got) || strings.Contains(string(got), "secret") {
		t.Fatalf("RedactJSONStrings() did not safely handle supported JSON depth: length=%d, prefix=%q", len(got), got[:min(len(got), 80)])
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

func TestRedactorJSONOutputDoesNotAliasCompactionToolArguments(t *testing.T) {
	raw := json.RawMessage(`{"path":"secret.go"}`)
	got := NewRedactor([]string{"secret"}).RedactJSONStrings(raw)
	got[0] = '['
	if string(raw) != `{"path":"secret.go"}` {
		t.Fatalf("redacted output aliases source tool arguments: %s", raw)
	}
}
