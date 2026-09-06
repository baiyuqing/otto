package server

import (
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"testing/fstest"
)

func TestUIIndexFromDist(t *testing.T) {
	dist := fstest.MapFS{"index.html": {Data: []byte("<!doctype html><title>otto</title>")}}
	index, _ := uiHandlers(dist)
	rec := httptest.NewRecorder()
	index.ServeHTTP(rec, httptest.NewRequest("GET", "/", nil))
	if rec.Code != 200 || !strings.Contains(rec.Body.String(), "<title>otto</title>") {
		t.Fatalf("status %d body %q", rec.Code, rec.Body.String())
	}
	if ct := rec.Header().Get("Content-Type"); ct != "text/html; charset=utf-8" {
		t.Fatalf("Content-Type = %q", ct)
	}
	if cc := rec.Header().Get("Cache-Control"); cc != "no-cache" {
		t.Fatalf("Cache-Control = %q", cc)
	}
}

func TestUIAssetsFromDist(t *testing.T) {
	dist := fstest.MapFS{"assets/app-abc123.js": {Data: []byte("console.log(1)")}}
	_, assets := uiHandlers(dist)
	rec := httptest.NewRecorder()
	assets.ServeHTTP(rec, httptest.NewRequest("GET", "/assets/app-abc123.js", nil))
	if rec.Code != 200 || rec.Body.String() != "console.log(1)" {
		t.Fatalf("status %d body %q", rec.Code, rec.Body.String())
	}
	if ct := rec.Header().Get("Content-Type"); !strings.HasPrefix(ct, "text/javascript") {
		t.Fatalf("Content-Type = %q", ct)
	}
}

func TestUIPlaceholderWhenNotBuilt(t *testing.T) {
	index, _ := uiHandlers(fstest.MapFS{})
	rec := httptest.NewRecorder()
	index.ServeHTTP(rec, httptest.NewRequest("GET", "/", nil))
	if rec.Code != 200 || rec.Body.String() != uiPlaceholder {
		t.Fatalf("status %d body %q", rec.Code, rec.Body.String())
	}
}

// The root is the page that carries the token to the browser, so it must be
// reachable without one; the API stays gated.
func TestRootServedWithoutToken(t *testing.T) {
	_, ts := newServerForTest(t, Options{Token: "secret-for-test"})
	resp, err := ts.Client().Get(ts.URL + "/")
	if err != nil {
		t.Fatal(err)
	}
	body, _ := io.ReadAll(resp.Body)
	resp.Body.Close()
	if resp.StatusCode != 200 || len(body) == 0 {
		t.Fatalf("GET / status %d body %q", resp.StatusCode, body)
	}
}

func TestUnknownPathStays404(t *testing.T) {
	_, ts := newServerForTest(t, Options{})
	for _, path := range []string{"/nope", "/v1/nope", "/assets/missing.js"} {
		resp, err := ts.Client().Get(ts.URL + path)
		if err != nil {
			t.Fatal(err)
		}
		resp.Body.Close()
		if resp.StatusCode != http.StatusNotFound {
			t.Errorf("GET %s status = %d, want 404", path, resp.StatusCode)
		}
	}
}
