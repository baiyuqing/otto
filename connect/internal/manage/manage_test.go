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

func TestClientReadsProfiles(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/v1/config/profiles" {
			t.Errorf("path = %s", r.URL.Path)
		}
		w.Header().Set("Content-Type", "application/json")
		_, _ = w.Write([]byte(`{"profiles":[{"name":"work","default":true,"provider":"openai-compatible","model":"small"}]}`))
	}))
	defer server.Close()
	profiles, err := NewWithBaseURL(server.Client(), server.URL).Profiles(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(profiles) != 1 || profiles[0].Name != "work" || !profiles[0].Default {
		t.Fatalf("profiles = %#v", profiles)
	}
}
