package tool

import (
	"strings"
	"testing"
	"unsafe"
)

func TestExactRedactingWriterUsesStreamingLeftmostLongestAcrossEverySplit(t *testing.T) {
	const marker = "[REDACTED]"
	tests := []struct {
		name     string
		input    string
		values   []string
		expected string
	}{
		{
			name:     "earlier partial beats later full match",
			input:    "credential-zzSHORT-rest",
			values:   []string{"credential-zzSHORT-rest", "SHORT"},
			expected: marker,
		},
		{
			name:     "equal start chooses longest match",
			input:    "before-abcdef-after",
			values:   []string{"abc", "abcdef", "bcde", "def"},
			expected: "before-" + marker + "-after",
		},
		{
			name:     "prefix suffix and substring set",
			input:    "xxTOKEN-tailTOKENyy",
			values:   []string{"TOKEN", "TOKEN-tail", "tailTOKEN", "OKEN", "TOKEN-tailTOKEN"},
			expected: "xx" + marker + "yy",
		},
		{
			name:     "overlap resolves from the left",
			input:    "zababaq",
			values:   []string{"aba", "bab", "ababa"},
			expected: "z" + marker + "q",
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			for split := 0; split <= len(test.input); split++ {
				got := redactTestChunks(t, test.values, test.input[:split], test.input[split:])
				if got != test.expected {
					t.Fatalf("split %d: redacted output = %q, want %q", split, got, test.expected)
				}
			}

			chunks := make([]string, len(test.input))
			for i := range test.input {
				chunks[i] = test.input[i : i+1]
			}
			if got := redactTestChunks(t, test.values, chunks...); got != test.expected {
				t.Fatalf("byte-by-byte output = %q, want %q", got, test.expected)
			}
		})
	}
}

func TestExactRedactingWriterHoldsSameStartLongerCandidateUntilFlush(t *testing.T) {
	var destination strings.Builder
	writer := newExactRedactingWriter(&destination, []string{"SHORT", "SHORT-rest"})
	if _, err := writer.Write([]byte("SHORT")); err != nil {
		t.Fatalf("Write() error = %v", err)
	}
	if got := destination.String(); got != "" {
		t.Fatalf("Write() emitted unresolved shorter match %q", got)
	}
	if err := writer.Flush(); err != nil {
		t.Fatalf("Flush() error = %v", err)
	}
	if got := destination.String(); got != "[REDACTED]" {
		t.Fatalf("flushed output = %q, want %q", got, "[REDACTED]")
	}
}

func TestExactRedactingWriterClonesConfiguredValues(t *testing.T) {
	const original = "mutable-secret"
	storage := []byte(original)
	aliased := unsafe.String(unsafe.SliceData(storage), len(storage))

	var destination strings.Builder
	writer := newExactRedactingWriter(&destination, []string{aliased})
	for i := range storage {
		storage[i] = 'x'
	}

	if _, err := writer.Write([]byte(original)); err != nil {
		t.Fatalf("Write() error = %v", err)
	}
	if err := writer.Flush(); err != nil {
		t.Fatalf("Flush() error = %v", err)
	}
	if got := destination.String(); got != "[REDACTED]" {
		t.Fatalf("redacted output = %q, want immutable configured value", got)
	}
}

func redactTestChunks(t *testing.T, values []string, chunks ...string) string {
	t.Helper()
	var destination strings.Builder
	writer := newExactRedactingWriter(&destination, values)
	for _, chunk := range chunks {
		if _, err := writer.Write([]byte(chunk)); err != nil {
			t.Fatalf("Write(%q) error = %v", chunk, err)
		}
	}
	if err := writer.Flush(); err != nil {
		t.Fatalf("Flush() error = %v", err)
	}
	return destination.String()
}
