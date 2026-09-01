package agent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"testing"
	"time"
	"unicode"
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

func TestRedactorCanonicalizesInvalidConfiguredValues(t *testing.T) {
	invalidFF := string(append([]byte("decoded-prefix"), 0xff))
	invalidOverlong := string(append([]byte("overlong-prefix"), 0xc0, 0xaf))
	canonical := []string{"decoded-prefix�", "overlong-prefix��"}
	redactor := NewRedactor([]string{invalidFF, invalidOverlong})

	for _, secret := range canonical {
		if got := redactor.RedactString("before " + secret + " after"); strings.Contains(got, secret) {
			t.Fatalf("RedactString() retained canonical configured value %q: %q", secret, got)
		}
	}

	stream := redactor.newStream()
	raw := append([]byte("decoded-prefix"), 0xff)
	raw = append(raw, []byte(" overlong-prefix")...)
	raw = append(raw, 0xc0, 0xaf)
	var streamed strings.Builder
	for _, value := range raw {
		streamed.WriteString(stream.Write(string([]byte{value})))
	}
	streamed.WriteString(stream.Flush())
	if !utf8.ValidString(streamed.String()) {
		t.Fatalf("stream returned invalid UTF-8: %q", streamed.String())
	}
	for _, secret := range canonical {
		if strings.Contains(streamed.String(), secret) {
			t.Fatalf("stream retained canonical configured value %q: %q", secret, streamed.String())
		}
	}

	boundaryErr := redactor.RedactError(errors.New(strings.Join(canonical, " | ")))
	for _, secret := range canonical {
		if strings.Contains(boundaryErr.Error(), secret) {
			t.Fatalf("RedactError() retained canonical configured value %q: %q", secret, boundaryErr)
		}
	}
}

func TestRedactorExpandsJSONStringEscapeFormsBeforeMarkerSelection(t *testing.T) {
	tests := []struct {
		raw     string
		decoded string
	}{
		{raw: `\u003c`, decoded: "<"},
		{raw: `\u003C`, decoded: "<"},
		{raw: `\u003e`, decoded: ">"},
		{raw: `\u0026`, decoded: "&"},
		{raw: `\"`, decoded: `"`},
		{raw: `\\`, decoded: `\`},
		{raw: `\/`, decoded: "/"},
		{raw: `\n`, decoded: "\n"},
		{raw: `\u2028`, decoded: "\u2028"},
		{raw: `\u2029`, decoded: "\u2029"},
	}
	excluded := make(map[rune]struct{})
	values := make([]string, 0, len(tests)+1)
	for _, test := range tests {
		for _, value := range test.decoded {
			excluded[value] = struct{}{}
		}
		values = append(values, test.raw)
	}
	values = append(values, allNonControlUnicodeRunesExcept(t, excluded))
	redactor := NewRedactor(values)

	for _, test := range tests {
		for _, input := range []string{test.raw, test.decoded} {
			redacted := redactor.RedactString(input)
			if strings.Contains(redacted, test.raw) || strings.Contains(redacted, test.decoded) {
				t.Fatalf("RedactString(%q) retained raw/decoded form: %q", input, redacted)
			}
			encoded, err := json.Marshal(redacted)
			if err != nil {
				t.Fatal(err)
			}
			if strings.Contains(string(encoded), test.raw) {
				t.Fatalf("JSON encoding recreated configured form %q: %s", test.raw, encoded)
			}
		}
	}
}

func TestRedactorCanonicalizesJSONWithoutConfiguredSecrets(t *testing.T) {
	raw := json.RawMessage(append([]byte(`"prefix`), 0xff, '"'))
	got := NewRedactor(nil).RedactJSONStrings(raw)
	if !json.Valid(got) || !utf8.Valid(got) || string(got) != `"prefix�"` {
		t.Fatalf("RedactJSONStrings() = %q", got)
	}
}

func TestRedactorCanonicalizesErrorsWithoutConfiguredSecrets(t *testing.T) {
	raw := errors.New(string([]byte{'e', 'r', 'r', 0xff, 0xc0, 0xaf}))
	got := NewRedactor(nil).RedactError(raw)
	if !utf8.ValidString(got.Error()) || got.Error() != "err���" {
		t.Fatalf("RedactError() = %q", got)
	}
}

func TestIncompleteRedactorSuppressesEveryDynamicBoundary(t *testing.T) {
	redactor := NewRedactorWithCompleteness([]string{"known-secret"}, false)
	if got := redactor.RedactString("unknown-attacker-content"); got != "" {
		t.Fatalf("RedactString() = %q, want fail-closed empty text", got)
	}
	if got := string(redactor.RedactJSONStrings(json.RawMessage(`{"unknown":"content"}`))); got != "null" {
		t.Fatalf("RedactJSONStrings() = %q, want fail-closed null", got)
	}
	boundaryErr := redactor.RedactError(fmt.Errorf("%w: unknown-attacker-content", context.Canceled))
	if boundaryErr == nil || boundaryErr.Error() != "" || !errors.Is(boundaryErr, context.Canceled) {
		t.Fatalf("RedactError() = %#v, want empty cancellation-preserving error", boundaryErr)
	}
	stream := redactor.newStream()
	if got := stream.Write("unknown-"); got != "" {
		t.Fatalf("stream Write() = %q, want no fail-closed event", got)
	}
	if got := stream.Write("attacker-content"); got != "" {
		t.Fatalf("stream Write() = %q, want no fail-closed event", got)
	}
	if got := stream.Flush(); got != "" {
		t.Fatalf("stream Flush() = %q, want no fail-closed event", got)
	}
}

func TestRedactorPreservesInvalidCompactionSummaryIdentity(t *testing.T) {
	err := fmt.Errorf("%w: response contains secret", ErrInvalidCompactionSummary)
	got := NewRedactor([]string{"secret"}).RedactError(err)
	if !errors.Is(got, ErrInvalidCompactionSummary) || strings.Contains(got.Error(), "secret") {
		t.Fatalf("RedactError() = %v", got)
	}
}

func TestRedactorUsesStableSingleRunePreferredMarkers(t *testing.T) {
	first := NewRedactor(nil)
	if first.marker != redactionMarker || utf8.RuneCountInString(first.marker) != 1 {
		t.Fatalf("default marker = %q, want stable single rune %q", first.marker, redactionMarker)
	}
	second := NewRedactor([]string{redactionMarker})
	if second.marker != "\ue000" || utf8.RuneCountInString(second.marker) != 1 {
		t.Fatalf("fallback marker = %q, want stable single-rune fallback", second.marker)
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

func TestRedactorExhaustiveMarkerFallbackSuppressesDynamicContentInConstantWork(t *testing.T) {
	allRunes := allNonControlUnicodeRunes(t)
	longPrefix := strings.Repeat("a", 1<<20) + "z"
	redactor := NewRedactor([]string{allRunes, longPrefix, "X", "ab"})
	if redactor.marker != "" || redactor.AllowsDynamicContent() {
		t.Fatalf("exhaustive marker fallback = marker %q allows-dynamic %t, want suppression", redactor.marker, redactor.AllowsDynamicContent())
	}

	const half = 50_000
	pathological := strings.Repeat("a", half) + "X" + strings.Repeat("b", half)
	done := make(chan string, 1)
	go func() { done <- redactor.RedactString(pathological) }()
	select {
	case got := <-done:
		if got != "" {
			t.Fatalf("RedactString() = %q, want suppressed output", got)
		}
	case <-time.After(500 * time.Millisecond):
		t.Fatal("unrepresentable RedactString exceeded constant-work deadline")
	}
	if allocations := testing.AllocsPerRun(10, func() {
		if got := redactor.RedactString(pathological); got != "" {
			t.Fatalf("RedactString() = %q", got)
		}
	}); allocations > 1 {
		t.Fatalf("unrepresentable RedactString allocations = %.1f, want <= 1", allocations)
	}

	streamInput := strings.Repeat("a", 8<<10) + "X" + strings.Repeat("b", 8<<10)
	if allocations := testing.AllocsPerRun(3, func() {
		stream := redactor.newStream()
		for index := range len(streamInput) {
			if got := stream.Write(streamInput[index : index+1]); got != "" {
				t.Fatalf("stream Write emitted %q", got)
			}
		}
		if got := stream.Flush(); got != "" {
			t.Fatalf("stream Flush() = %q", got)
		}
	}); allocations > 4 {
		t.Fatalf("one-byte suppressed stream allocations = %.1f, want <= 4", allocations)
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

func allNonControlUnicodeRunesExcept(t *testing.T, excluded map[rune]struct{}) string {
	t.Helper()
	var value strings.Builder
	for candidate := rune(1); candidate <= utf8.MaxRune; candidate++ {
		if !utf8.ValidRune(candidate) || unicode.IsControl(candidate) {
			continue
		}
		if _, skip := excluded[candidate]; skip {
			continue
		}
		value.WriteRune(candidate)
	}
	result := value.String()
	if !utf8.ValidString(result) || !strings.Contains(result, redactionMarker) {
		t.Fatal("marker-selection fixture is invalid or omitted the preferred rune")
	}
	return result
}

func allNonControlUnicodeRunes(t *testing.T) string {
	t.Helper()
	var value strings.Builder
	for candidate := rune(1); candidate <= utf8.MaxRune; candidate++ {
		if !utf8.ValidRune(candidate) || unicode.IsControl(candidate) {
			continue
		}
		value.WriteRune(candidate)
	}
	result := value.String()
	if !utf8.ValidString(result) || !strings.Contains(result, redactionMarker) {
		t.Fatal("exhaustive marker fixture is invalid or omitted the preferred rune")
	}
	return result
}
