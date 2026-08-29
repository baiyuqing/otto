package openaicompat

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/provider"
)

func TestNewDefaultClientIsHardened(t *testing.T) {
	client := New("https://example.test/v1", "key", nil)
	if client.httpClient == http.DefaultClient {
		t.Fatal("nil httpClient must not fall back to the unconfigured http.DefaultClient")
	}
	transport, ok := client.httpClient.Transport.(*http.Transport)
	if !ok || transport == nil {
		t.Fatalf("default transport = %T, want *http.Transport", client.httpClient.Transport)
	}
	if transport.DialContext == nil {
		t.Fatal("default transport has no dial timeout")
	}
	if transport.TLSHandshakeTimeout <= 0 {
		t.Fatalf("TLSHandshakeTimeout = %v, want > 0", transport.TLSHandshakeTimeout)
	}
	if transport.ResponseHeaderTimeout <= 0 {
		t.Fatalf("ResponseHeaderTimeout = %v, want > 0", transport.ResponseHeaderTimeout)
	}
	if transport.MaxResponseHeaderBytes <= 0 {
		t.Fatalf("MaxResponseHeaderBytes = %v, want > 0", transport.MaxResponseHeaderBytes)
	}
	if client.httpClient.CheckRedirect == nil {
		t.Fatal("default client has no redirect policy")
	}
}

func TestDefaultClientBlocksCrossOriginRedirect(t *testing.T) {
	targetCalls := 0
	target := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		targetCalls++
		http.Error(w, "must not be reached", http.StatusInternalServerError)
	}))
	defer target.Close()

	sourceCalls := 0
	source := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		sourceCalls++
		http.Redirect(w, r, target.URL+"/chat/completions", http.StatusFound)
	}))
	defer source.Close()

	client := New(source.URL, "secret", nil)
	client.sleep = noSleep
	_, err := client.Complete(context.Background(), provider.Request{Model: "model"}, nil)
	if err == nil {
		t.Fatal("expected cross-origin redirect to be rejected")
	}
	if !strings.Contains(err.Error(), "redirect") {
		t.Fatalf("error = %v, want redirect policy error", err)
	}
	if sourceCalls != 1 || targetCalls != 0 {
		t.Fatalf("sourceCalls = %d, targetCalls = %d, want 1 and 0", sourceCalls, targetCalls)
	}
}

func TestDefaultClientFollowsSameOriginRedirect(t *testing.T) {
	var gotAuthorization string
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path == "/redirect/chat/completions" {
			http.Redirect(w, r, "/v1/chat/completions", http.StatusFound)
			return
		}
		if r.URL.Path != "/v1/chat/completions" {
			http.Error(w, "unexpected path", http.StatusNotFound)
			return
		}
		gotAuthorization = r.Header.Get("Authorization")
		w.Header().Set("Content-Type", "text/event-stream")
		fmt.Fprint(w, "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
	}))
	defer server.Close()

	client := New(server.URL+"/redirect", "secret", nil)
	if _, err := client.Complete(context.Background(), provider.Request{Model: "model"}, nil); err != nil {
		t.Fatal(err)
	}
	if gotAuthorization != "Bearer secret" {
		t.Fatalf("Authorization = %q, want preserved across same-origin redirect", gotAuthorization)
	}
}
