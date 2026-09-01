package auth

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
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
	ErrInteractiveUnavailable   = errors.New("chatgpt sign-in is unavailable in this session")
)

const maxCredentialFileBytes = 1 << 20

func boundedAuthError(kind, cause error) error {
	if cause == nil {
		return nil
	}
	return kind
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
	file, err := os.Open(path)
	if err != nil {
		if os.IsNotExist(err) {
			return Credentials{}, ErrNoCredentials
		}
		return Credentials{}, boundedAuthError(ErrCredentialsUnavailable, err)
	}
	defer file.Close()

	data, err := io.ReadAll(io.LimitReader(file, maxCredentialFileBytes+1))
	if err != nil {
		return Credentials{}, boundedAuthError(ErrCredentialsUnavailable, err)
	}
	if len(data) > maxCredentialFileBytes {
		return Credentials{}, boundedAuthError(ErrCredentialsUnavailable, errors.New("credentials exceed maximum size"))
	}
	var creds Credentials
	if err := json.Unmarshal(data, &creds); err != nil {
		return Credentials{}, boundedAuthError(ErrCredentialsUnavailable, err)
	}
	if !credentialsWithinBounds(creds) {
		return Credentials{}, boundedAuthError(ErrCredentialsUnavailable, errors.New("credentials exceed maximum size"))
	}
	return creds, nil
}

// Save writes credentials to path atomically with 0600 permissions, creating
// parent directories (0700) as needed.
func (c Credentials) Save(path string) error {
	if !credentialsWithinBounds(c) {
		return boundedAuthError(ErrCredentialsPersistence, errors.New("credentials exceed maximum size"))
	}
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return boundedAuthError(ErrCredentialsPersistence, err)
	}
	data, err := json.MarshalIndent(c, "", "  ")
	if err != nil || len(data) > maxCredentialFileBytes {
		return boundedAuthError(ErrCredentialsPersistence, errors.New("credentials exceed maximum size"))
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

func credentialsWithinBounds(creds Credentials) bool {
	total := 0
	for _, value := range []string{creds.AccessToken, creds.RefreshToken, creds.IDToken, creds.AccountID} {
		if len(value) > maxCredentialFileBytes-total {
			return false
		}
		total += len(value)
	}
	return true
}
