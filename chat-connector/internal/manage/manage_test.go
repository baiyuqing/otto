package manage

import (
	"context"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestFromCommandRequiresAttach(t *testing.T) {
	if _, err := FromCommand([]string{"otto", "acp"}, t.TempDir()); err == nil {
		t.Fatal("direct ACP command unexpectedly enabled management")
	}
}

func TestClientReadsProviders(t *testing.T) {
	server := newTestServer(t, func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/config/providers" {
			t.Errorf("path = %s", r.URL.Path)
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"default_model":"work/small","providers":[{"name":"work","type":"openai-compatible","builtin":false,` +
			`"base_url":"https://w.example/v1","models":[{"model_id":"small","selector":"work/small","default":true,"thinking":"high"}]}]}`))
	})
	out, err := NewWithBaseURL(server.client, server.url).Providers(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if out.DefaultModel != "work/small" || len(out.Providers) != 1 {
		t.Fatalf("providers = %#v", out)
	}
	model := out.Providers[0].Models[0]
	if model.Selector != "work/small" || !model.Default || model.Thinking != "high" || out.Providers[0].BaseURL != "https://w.example/v1" {
		t.Fatalf("provider = %#v", out.Providers[0])
	}
}

func TestClientModelsQueriesTheProvider(t *testing.T) {
	server := newTestServer(t, func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/config/models" || r.URL.Query().Get("provider") != "a b" {
			t.Errorf("url = %s", r.URL)
		}
		_, _ = w.Write([]byte(`{"provider":"a b","models":["m1","m2"]}`))
	})
	ids, err := NewWithBaseURL(server.client, server.url).Models(context.Background(), "a b")
	if err != nil || len(ids) != 2 {
		t.Fatalf("ids = %v, err = %v", ids, err)
	}
}

func TestClientPreviewDecodesTarget(t *testing.T) {
	server := newTestServer(t, func(w http.ResponseWriter, r *http.Request) {
		_, _ = w.Write([]byte(`{"id":"t1","expires_in_seconds":60,"operation":"set_model","target":"p/m","diff":"d"}`))
	})
	preview, err := NewWithBaseURL(server.client, server.url).Preview(context.Background(), map[string]any{"kind": "set_model"})
	if err != nil || preview.Target != "p/m" || preview.ID != "t1" || preview.ExpiresInSeconds != 60 {
		t.Fatalf("preview = %#v, err = %v", preview, err)
	}
}

// testServer calls a handler in-process so tests need no listening port.
type testServer struct {
	client *http.Client
	url    string
}

type recorderTransport struct{ handler http.Handler }

func (t recorderTransport) RoundTrip(r *http.Request) (*http.Response, error) {
	rec := httptest.NewRecorder()
	t.handler.ServeHTTP(rec, r)
	return rec.Result(), nil
}

func newTestServer(t *testing.T, handler http.HandlerFunc) testServer {
	t.Helper()
	return testServer{client: &http.Client{Transport: recorderTransport{handler}}, url: "http://otto"}
}
