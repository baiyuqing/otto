package auth

import (
	"errors"
	"fmt"
)

// StatusLine reports whether ChatGPT credentials exist at path and returns a
// single-line, human-readable summary for the interactive frontends. It never
// includes token values.
func StatusLine(path string) (line string, signedIn bool) {
	creds, err := Load(path)
	if err != nil {
		if errors.Is(err, ErrNoCredentials) {
			return "Not signed in to ChatGPT. Run 'otto login'.", false
		}
		return "ChatGPT sign-in state is unavailable. Run 'otto login'.", false
	}
	line = "Signed in to ChatGPT."
	if !creds.Expiry.IsZero() {
		line += fmt.Sprintf(" Access token expires %s.", creds.Expiry.Format("2006-01-02 15:04:05 MST"))
	}
	return line, true
}
