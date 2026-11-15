package bridge

import (
	"encoding/json"
	"io"
	"net/http"
	"reflect"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
)

// recorded is one request seen by a management test server.
type recorded struct{ method, path, query, body string }

// replyWith serves a fixed status and JSON body and records every request.
func replyWith(status int, body string, seen *[]recorded) http.HandlerFunc {
	var mu sync.Mutex
	return func(w http.ResponseWriter, r *http.Request) {
		raw, _ := io.ReadAll(r.Body)
		mu.Lock()
		*seen = append(*seen, recorded{r.Method, r.URL.Path, r.URL.RawQuery, string(raw)})
		mu.Unlock()
		w.Header().Set("Content-Type", "application/json")
		w.WriteHeader(status)
		_, _ = w.Write([]byte(body))
	}
}

const providersJSON = `{"default_model":"chatgpt/gpt-5.6-luna","providers":[` +
	`{"name":"chatgpt","type":"chatgpt","builtin":true,"models":[` +
	`{"model_id":"gpt-5.6-luna","selector":"chatgpt/gpt-5.6-luna","default":true},` +
	`{"model_id":"gpt-5.6-terra","selector":"chatgpt/gpt-5.6-terra","default":false,"thinking":"high"}]},` +
	`{"name":"deepseek","type":"openai-compatible","builtin":false,"base_url":"https://api.deepseek.example/v1","api_key_env":"DEEPSEEK_API_KEY","models":[]}]}`

func TestConfigModelsListsSelectorsAndMarksTheDefault(t *testing.T) {
	for _, cmd := range []string{"/config", "/config models"} {
		var seen []recorded
		h := newHarness(t, setup{manage: managementServer(t, replyWith(200, providersJSON, &seen))})
		h.say(cmd)
		got := h.waitSent(1)
		want := "Configured models:\n• chatgpt/gpt-5.6-luna (default)\n• chatgpt/gpt-5.6-terra\n" +
			"Use /config show <provider>, /models <provider>, /config use <provider>/<model_id>, or /config add <provider>/<model_id>."
		if got[0] != want {
			t.Errorf("%s reply = %q, want %q", cmd, got[0], want)
		}
		if len(seen) != 1 || seen[0].path != "/v1/config/providers" {
			t.Errorf("%s requests = %+v", cmd, seen)
		}
		settle()
		if calls := agentWork(h.dir); len(calls) != 0 {
			t.Errorf("%s reached the agent: %q", cmd, calls)
		}
	}
}

func TestConfigShowPrintsTheProviderAndItsModels(t *testing.T) {
	var seen []recorded
	body := `{"name":"deepseek","type":"openai-compatible","builtin":false,"base_url":"https://api.deepseek.example/v1",` +
		`"api_key_env":"DEEPSEEK_API_KEY","models":[{"model_id":"deepseek-chat","selector":"deepseek/deepseek-chat","default":false,"thinking":"high"}]}`
	h := newHarness(t, setup{manage: managementServer(t, replyWith(200, body, &seen))})
	h.say("/config show deepseek")
	got := h.waitSent(1)
	want := "deepseek\ntype: openai-compatible\nbase_url: https://api.deepseek.example/v1\napi_key_env: DEEPSEEK_API_KEY\n• deepseek/deepseek-chat (thinking high)"
	if got[0] != want {
		t.Errorf("reply = %q, want %q", got[0], want)
	}
	if len(seen) != 1 || seen[0].path != "/v1/config/providers/deepseek" {
		t.Errorf("requests = %+v", seen)
	}
}

func TestConfigShowUnknownProviderIsReported(t *testing.T) {
	var seen []recorded
	h := newHarness(t, setup{manage: managementServer(t, replyWith(404, `{"error":{"code":"provider_not_found","message":"x"}}`, &seen))})
	h.say("/config show nope")
	if got := h.waitSent(1); got[0] != "Provider not found. Send /config models." {
		t.Errorf("reply = %q", got[0])
	}
}

func TestModelsListsRemoteIDsAsSelectors(t *testing.T) {
	var seen []recorded
	h := newHarness(t, setup{manage: managementServer(t, replyWith(200, `{"provider":"deepseek","models":["a","b"]}`, &seen))})
	h.say("/models deepseek")
	if got := h.waitSent(1); got[0] != "Models of deepseek:\ndeepseek/a\ndeepseek/b" {
		t.Errorf("reply = %q", got[0])
	}
	if len(seen) != 1 || seen[0].path != "/v1/config/models" || seen[0].query != "provider=deepseek" {
		t.Errorf("requests = %+v", seen)
	}
}

func TestModelsErrorsAreMappedToPlainReplies(t *testing.T) {
	for code, want := range map[string]string{
		"models_unsupported": "The chatgpt provider cannot list models; use a model ID your account can use.",
		"provider_not_found": "Provider not found. Send /config models.",
	} {
		var seen []recorded
		h := newHarness(t, setup{manage: managementServer(t, replyWith(501, `{"error":{"code":"`+code+`","message":"x"}}`, &seen))})
		h.say("/models chatgpt")
		if got := h.waitSent(1); got[0] != want {
			t.Errorf("%s reply = %q, want %q", code, got[0], want)
		}
	}
}

func TestModelsWithoutAValidProviderShowsUsage(t *testing.T) {
	var hits atomic.Int32
	h := newHarness(t, setup{manage: managementServer(t, func(w http.ResponseWriter, r *http.Request) { hits.Add(1) })})
	h.say("/models")
	if got := h.waitSent(1); !strings.HasPrefix(got[0], "Usage: /models <provider>") {
		t.Errorf("reply = %q", got[0])
	}
	if hits.Load() != 0 {
		t.Error("an invalid provider reached Otto")
	}
}

const previewJSON = `{"id":"tok1","expires_in_seconds":60,"operation":"op","target":"chatgpt/gpt-5.6-terra","diff":"the diff"}`

func TestConfigChangesSendTheSelectorKinds(t *testing.T) {
	for cmd, want := range map[string]string{
		"/config use chatgpt/gpt-5.6-terra":                         `{"kind":"set_default_model","model":"chatgpt/gpt-5.6-terra"}`,
		"/config remove chatgpt/gpt-5.6-terra":                      `{"kind":"remove_model","model":"chatgpt/gpt-5.6-terra"}`,
		"/config add chatgpt/gpt-5.6-terra":                         `{"kind":"set_model","model":"chatgpt/gpt-5.6-terra"}`,
		"/config add chatgpt/gpt-5.6-terra --thinking high":         `{"kind":"set_model","model":"chatgpt/gpt-5.6-terra","thinking":"high"}`,
		"/config set openrouter/anthropic/claude-x thinking medium": `{"kind":"set_model_field","model":"openrouter/anthropic/claude-x","field":"thinking","value":"medium"}`,
	} {
		var seen []recorded
		h := newHarness(t, setup{manage: managementServer(t, replyWith(200, previewJSON, &seen))})
		h.say(cmd)
		got := h.waitSent(1)
		if got[0] != "Preview: the diff. Confirm with /config confirm tok1 within 60 seconds." {
			t.Errorf("%s reply = %q", cmd, got[0])
		}
		if len(seen) != 1 || seen[0].method != "POST" || seen[0].path != "/v1/config/changes" || !jsonEqual(seen[0].body, want) {
			t.Errorf("%s requests = %+v, want body %s", cmd, seen, want)
		}
	}
}

func TestConfigRejectsWhatIsNotASelectorWithoutCallingOtto(t *testing.T) {
	for _, cmd := range []string{
		"/config use terra", "/config remove a b/c d", "/config add terra", "/config set terra thinking high",
		"/config use /m", "/config add p/",
	} {
		var hits atomic.Int32
		h := newHarness(t, setup{manage: managementServer(t, func(w http.ResponseWriter, r *http.Request) { hits.Add(1) })})
		h.say(cmd)
		got := h.waitSent(1)
		if !strings.HasPrefix(got[0], "Expected a model selector") && !strings.HasPrefix(got[0], "Usage:") {
			t.Errorf("%s reply = %q", cmd, got[0])
		}
		settle()
		if hits.Load() != 0 {
			t.Errorf("%q reached Otto", cmd)
		}
	}
}

func TestConfigChatCannotSetProviderConnectionFields(t *testing.T) {
	for _, cmd := range []string{
		"/config add p/m --base-url https://evil.example",
		"/config add p/m --api-key-env AWS_SECRET_ACCESS_KEY",
		"/config add p/m --provider chatgpt",
		"/config set p/m base-url https://evil.example",
		"/config set p/m base_url https://evil.example",
		"/config set p/m api-key-env AWS_SECRET_ACCESS_KEY",
		"/config set p/m api_key_env AWS_SECRET_ACCESS_KEY",
	} {
		var hits atomic.Int32
		h := newHarness(t, setup{manage: managementServer(t, func(w http.ResponseWriter, r *http.Request) { hits.Add(1) })})
		h.say(cmd)
		if got := h.waitSent(1); !strings.Contains(got[0], "only be changed on the host") {
			t.Errorf("%s reply = %q", cmd, got[0])
		}
		settle()
		if hits.Load() != 0 {
			t.Errorf("%q reached Otto", cmd)
		}
	}
}

func TestConfigSetOtherFieldsAreRefused(t *testing.T) {
	var hits atomic.Int32
	h := newHarness(t, setup{manage: managementServer(t, func(w http.ResponseWriter, r *http.Request) { hits.Add(1) })})
	h.say("/config set p/m model other")
	if got := h.waitSent(1); got[0] != "Only thinking can be changed." {
		t.Errorf("reply = %q", got[0])
	}
	if hits.Load() != 0 {
		t.Error("an unsupported field reached Otto")
	}
}

func TestConfigChangeErrorsAreMappedToPlainReplies(t *testing.T) {
	for code, want := range map[string]string{
		"model_in_use":       "That model is the default model or the approval reviewer model. Select another model first.",
		"provider_not_found": "Provider not found. Send /config models.",
	} {
		var seen []recorded
		h := newHarness(t, setup{manage: managementServer(t, replyWith(409, `{"error":{"code":"`+code+`","message":"x"}}`, &seen))})
		h.say("/config remove chatgpt/gpt-5.6-luna")
		got := h.waitSent(1)
		if !strings.HasPrefix(got[0], want) {
			t.Errorf("%s reply = %q, want prefix %q", code, got[0], want)
		}
	}
}

func TestConfigUnknownSubcommandShowsUsage(t *testing.T) {
	var seen []recorded
	h := newHarness(t, setup{manage: managementServer(t, replyWith(200, `{}`, &seen))})
	h.say("/config profiles")
	got := h.waitSent(1)
	if got[0] != configUsage || len(seen) != 0 {
		t.Errorf("reply = %q, requests = %+v", got[0], seen)
	}
}

// jsonEqual compares two JSON documents ignoring key order and spacing.
func jsonEqual(a, b string) bool {
	var x, y any
	if json.Unmarshal([]byte(a), &x) != nil || json.Unmarshal([]byte(b), &y) != nil {
		return false
	}
	return reflect.DeepEqual(x, y)
}
