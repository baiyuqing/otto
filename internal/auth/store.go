package auth

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"time"
)

// ErrNoCredentials is returned by Load when no credential file exists. Callers
// use it to prompt the user to run "otto login".
var ErrNoCredentials = errors.New("no chatgpt credentials; run 'otto login'")

var (
	ErrCredentialsUnavailable   = errors.New("chatgpt credentials are unavailable; run 'otto login'")
	ErrCredentialsPersistence   = errors.New("chatgpt credentials could not be saved")
	ErrAccessTokenRefreshFailed = errors.New("chatgpt access token refresh failed; run 'otto login'")
)

func boundedAuthError(kind, cause error) error {
	if cause == nil {
		return nil
	}
	return authError{kind: kind, cause: cause}
}

type authError struct {
	kind  error
	cause error
}

func (e authError) Error() string {
	if e.kind == nil {
		return ""
	}
	return e.kind.Error()
}

func (e authError) Unwrap() error {
	return e.cause
}

func (e authError) Is(target error) bool {
	return target == e.kind || errors.Is(e.cause, target)
}

// Credentials holds the tokens obtained from the ChatGPT OAuth flow. It is
// persisted as JSON with 0600 permissions and never logged.
type Credentials struct {
	AccessToken  string    `json:"access_token"`
	RefreshToken string    `json:"refresh_token"`
	IDToken      string    `json:"id_token"`
	AccountID    string    `json:"account_id"`
	Expiry       time.Time `json:"expiry"`
}

// DefaultPath is ~/.otto/auth/chatgpt.json.
func DefaultPath() (string, error) {
	home, err := os.UserHomeDir()
	if err != nil {
		return "", fmt.Errorf("resolve home directory: %w", err)
	}
	return PathForHome(home), nil
}

// PathForHome returns the credential path under the given home directory.
func PathForHome(home string) string {
	return filepath.Join(home, ".otto", "auth", "chatgpt.json")
}

// Load reads credentials from path, returning ErrNoCredentials if the file is
// absent.
func Load(path string) (Credentials, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return Credentials{}, ErrNoCredentials
		}
		return Credentials{}, boundedAuthError(ErrCredentialsUnavailable, err)
	}
	var creds Credentials
	if err := json.Unmarshal(data, &creds); err != nil {
		return Credentials{}, boundedAuthError(ErrCredentialsUnavailable, err)
	}
	return creds, nil
}

// Save writes credentials to path atomically with 0600 permissions, creating
// parent directories (0700) as needed.
func (c Credentials) Save(path string) error {
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return boundedAuthError(ErrCredentialsPersistence, err)
	}
	data, err := json.MarshalIndent(c, "", "  ")
	if err != nil {
		return boundedAuthError(ErrCredentialsPersistence, err)
	}
	tmp, err := os.CreateTemp(dir, ".chatgpt-*.tmp")
	if err != nil {
		return boundedAuthError(ErrCredentialsPersistence, err)
	}
	tmpName := tmp.Name()
	defer os.Remove(tmpName)
	if err := tmp.Chmod(0o600); err != nil {
		_ = tmp.Close()
		return boundedAuthError(ErrCredentialsPersistence, err)
	}
	if _, err := tmp.Write(data); err != nil {
		_ = tmp.Close()
		return boundedAuthError(ErrCredentialsPersistence, err)
	}
	if err := tmp.Close(); err != nil {
		return boundedAuthError(ErrCredentialsPersistence, err)
	}
	if err := os.Rename(tmpName, path); err != nil {
		return boundedAuthError(ErrCredentialsPersistence, err)
	}
	return nil
}
