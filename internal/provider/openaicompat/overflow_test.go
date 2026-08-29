package openaicompat

import (
	"errors"
	"net/http"
	"strconv"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/provider"
)

func TestContextOverflowClassifierAcceptsAllowlistedStructuredEvidence(t *testing.T) {
	tests := []struct {
		name string
		body string
		code string
	}{
		{name: "nested code", body: `{"error":{"code":"context_length_exceeded"}}`, code: "context_length_exceeded"},
		{name: "root code", body: `{"code":"context_window_exceeded"}`, code: "context_window_exceeded"},
		{name: "nested type", body: `{"error":{"type":"max_context_length"}}`, code: "max_context_length"},
		{name: "root type case folded", body: `{"type":"CONTEXT_LENGTH_EXCEEDED"}`, code: "context_length_exceeded"},
		{name: "allowlisted code with output param", body: `{"error":{"code":"context_length_exceeded","param":"max_tokens"}}`, code: "context_length_exceeded"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			overflow := classifyContextOverflow(http.StatusBadRequest, []byte(test.body))
			if overflow == nil || overflow.Status != http.StatusBadRequest || overflow.Code != test.code {
				t.Fatalf("classifyContextOverflow() = %#v", overflow)
			}
			if !errors.Is(overflow, provider.ErrContextOverflow) {
				t.Fatal("classified error lost overflow identity")
			}
		})
	}
}

func TestContextOverflowClassifierAcceptsOnlyNarrowMessages(t *testing.T) {
	tests := []struct {
		name string
		body string
		want bool
	}{
		{name: "maximum context length", body: `{"error":{"message":"Maximum Context Length is 128000 tokens"}}`, want: true},
		{name: "context window exceeded with newline", body: `{"message":"the CONTEXT WINDOW\nwas exceeded"}`, want: true},
		{name: "input tokens exceed context", body: `{"error":{"message":"Input tokens in this request exceed the available context size"}}`, want: true},
		{name: "output max tokens", body: `{"error":{"code":"max_tokens","message":"max_tokens exceeds the maximum context length; requested 5000 output tokens"}}`},
		{name: "nested output max tokens param", body: `{"error":{"param":"max_tokens","message":"maximum context length is 128000 tokens"}}`},
		{name: "root output max tokens param", body: `{"param":"max_tokens","message":"maximum context length is 128000 tokens"}`},
		{name: "non-string output param", body: `{"error":{"param":123,"message":"maximum context length is 128000 tokens"}}`, want: true},
		{name: "generic context", body: `{"error":{"message":"context is too large"}}`},
		{name: "window without exceeded", body: `{"message":"context window is available"}`},
		{name: "output tokens", body: `{"message":"output tokens exceed the context window"}`},
		{name: "phrase gap too wide", body: `{"message":"context window ` + strings.Repeat("x", 65) + ` exceeded"}`},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			got := classifyContextOverflow(http.StatusUnprocessableEntity, []byte(test.body)) != nil
			if got != test.want {
				t.Fatalf("classified = %v, want %v", got, test.want)
			}
		})
	}
}

func TestContextOverflowClassifierExtractsOnlyBoundedTokenPairs(t *testing.T) {
	tests := []struct {
		name        string
		message     string
		wantCurrent int
		wantMaximum int
	}{
		{
			name:        "maximum then requested provider wording",
			message:     "maximum context length is 128000 tokens; requested 130000 tokens",
			wantCurrent: 130000,
			wantMaximum: 128000,
		},
		{
			name:        "requested then maximum",
			message:     "requested 130000 tokens; maximum 128000",
			wantCurrent: 130000,
			wantMaximum: 128000,
		},
		{
			name:        "token count then context maximum",
			message:     "130000 tokens were supplied; maximum context length is 128000",
			wantCurrent: 130000,
			wantMaximum: 128000,
		},
		{
			name:        "pair at distance boundary",
			message:     "requested 130000 tokens" + strings.Repeat(" ", 64) + "maximum 128000; maximum context length",
			wantCurrent: 130000,
			wantMaximum: 128000,
		},
		{
			name:    "pair beyond distance boundary",
			message: "requested 130000 tokens" + strings.Repeat(" ", 65) + "maximum 128000; maximum context length",
		},
		{
			name:    "unrelated numbers",
			message: "maximum context length: account 7, request 9, limit 11",
		},
		{
			name:    "integer overflow",
			message: "requested " + strings.Repeat("9", 100) + " tokens; maximum 128000; maximum context length",
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			body := []byte(`{"error":{"code":"context_length_exceeded","message":` + strconv.Quote(test.message) + `}}`)
			overflow := classifyContextOverflow(http.StatusRequestEntityTooLarge, body)
			if overflow == nil {
				t.Fatal("expected context overflow classification")
			}
			if overflow.CurrentTokens != test.wantCurrent || overflow.MaximumTokens != test.wantMaximum {
				t.Fatalf("counts = (%d, %d), want (%d, %d)", overflow.CurrentTokens, overflow.MaximumTokens, test.wantCurrent, test.wantMaximum)
			}
		})
	}
}

func TestContextOverflowClassifierRejectsStatusesMalformedAndDuplicateJSON(t *testing.T) {
	valid := []byte(`{"error":{"code":"context_length_exceeded"}}`)
	for _, status := range []int{http.StatusOK, http.StatusUnauthorized, http.StatusTooManyRequests, http.StatusInternalServerError} {
		t.Run(strconv.Itoa(status), func(t *testing.T) {
			if got := classifyContextOverflow(status, valid); got != nil {
				t.Fatalf("classifyContextOverflow() = %#v", got)
			}
		})
	}

	for _, body := range []string{
		`{"code":" context_length_exceeded "}`,
		`{"type":"context_length_exceeded_extra"}`,
		`{"error":{"code":123,"message":"ordinary validation failure"}}`,
		`{"error":{"code":"context_length_exceeded"}`,
		`{"error":{"code":"other","code":"context_length_exceeded"}}`,
		`{"error":{"code":"other","\u0063ode":"context_length_exceeded"}}`,
		`{"message":"safe","message":"maximum context length"}`,
		`{"param":"other","param":"max_tokens","message":"maximum context length"}`,
		`{"code":"context_length_exceeded"} {}`,
	} {
		t.Run(body, func(t *testing.T) {
			if got := classifyContextOverflow(http.StatusBadRequest, []byte(body)); got != nil {
				t.Fatalf("classifyContextOverflow() = %#v", got)
			}
		})
	}
}

func TestContextOverflowClassifierErrorNeverRetainsProviderBody(t *testing.T) {
	const secret = "body-secret-value"
	body := []byte(`{"error":{"message":"maximum context length; ` + secret + `"}}`)
	overflow := classifyContextOverflow(http.StatusBadRequest, body)
	if overflow == nil {
		t.Fatal("expected context overflow classification")
	}
	if strings.Contains(overflow.Error(), secret) || strings.Contains(overflow.Error(), string(body)) {
		t.Fatalf("typed error leaked provider body: %q", overflow.Error())
	}
}
