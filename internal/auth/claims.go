package auth

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
	"strings"
)

// openaiAuthClaim is the namespaced claim object OpenAI embeds in the ID token.
const openaiAuthClaim = "https://api.openai.com/auth"

// accountIDFromIDToken extracts chatgpt_account_id from an OpenAI ID token.
//
// The token is a JWT (header.payload.signature). Only the payload is decoded;
// the signature is not verified because the token was just received directly
// from the OAuth token endpoint over TLS and is used solely to read the account
// id needed to route requests. The claim is normally nested under the
// namespaced "https://api.openai.com/auth" object; a top-level key is accepted
// as a fallback.
func accountIDFromIDToken(idToken string) (string, error) {
	parts := strings.Split(idToken, ".")
	if len(parts) < 2 {
		return "", fmt.Errorf("malformed id token: want at least 2 segments, got %d", len(parts))
	}
	payload, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		return "", fmt.Errorf("decode id token payload: %w", err)
	}
	var claims map[string]json.RawMessage
	if err := json.Unmarshal(payload, &claims); err != nil {
		return "", fmt.Errorf("parse id token claims: %w", err)
	}

	if raw, ok := claims[openaiAuthClaim]; ok {
		var nested struct {
			AccountID string `json:"chatgpt_account_id"`
		}
		if err := json.Unmarshal(raw, &nested); err == nil && nested.AccountID != "" {
			return nested.AccountID, nil
		}
	}
	if raw, ok := claims["chatgpt_account_id"]; ok {
		var id string
		if err := json.Unmarshal(raw, &id); err == nil && id != "" {
			return id, nil
		}
	}
	return "", fmt.Errorf("id token has no chatgpt_account_id claim")
}
