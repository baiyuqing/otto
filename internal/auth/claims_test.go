package auth

import (
	"encoding/base64"
	"encoding/json"
	"strings"
	"testing"
)

// fakeIDToken builds an unsigned JWT (header.payload.signature) whose payload
// is the given claims map. The signature segment is arbitrary because the
// parser never verifies it.
func fakeIDToken(t *testing.T, claims map[string]any) string {
	t.Helper()
	header := base64.RawURLEncoding.EncodeToString([]byte(`{"alg":"none","typ":"JWT"}`))
	body, err := json.Marshal(claims)
	if err != nil {
		t.Fatalf("marshal claims: %v", err)
	}
	payload := base64.RawURLEncoding.EncodeToString(body)
	return strings.Join([]string{header, payload, "sig"}, ".")
}

func TestAccountIDFromIDTokenNestedClaim(t *testing.T) {
	token := fakeIDToken(t, map[string]any{
		"https://api.openai.com/auth": map[string]any{
			"chatgpt_account_id": "acct-123",
			"chatgpt_plan_type":  "pro",
		},
	})
	got, err := accountIDFromIDToken(token)
	if err != nil {
		t.Fatalf("accountIDFromIDToken: %v", err)
	}
	if got != "acct-123" {
		t.Fatalf("account id = %q, want acct-123", got)
	}
}

func TestAccountIDFromIDTokenTopLevelFallback(t *testing.T) {
	token := fakeIDToken(t, map[string]any{"chatgpt_account_id": "top-999"})
	got, err := accountIDFromIDToken(token)
	if err != nil {
		t.Fatalf("accountIDFromIDToken: %v", err)
	}
	if got != "top-999" {
		t.Fatalf("account id = %q, want top-999", got)
	}
}

func TestAccountIDFromIDTokenMissing(t *testing.T) {
	token := fakeIDToken(t, map[string]any{"sub": "user"})
	if _, err := accountIDFromIDToken(token); err == nil {
		t.Fatal("expected error when chatgpt_account_id is absent")
	}
}

func TestAccountIDFromIDTokenMalformed(t *testing.T) {
	if _, err := accountIDFromIDToken("not-a-jwt"); err == nil {
		t.Fatal("expected error for malformed token")
	}
}
