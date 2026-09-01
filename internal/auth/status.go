package auth

import "fmt"

// StatusLine reports whether ChatGPT credentials exist at path and returns a
// single-line, human-readable summary for the interactive frontends. It never
// includes token values.
func StatusLine(path string) (line string, signedIn bool) {
	creds, err := Load(path)
	if err != nil {
		return "Not signed in to ChatGPT. Run 'otto login'.", false
	}
	line = fmt.Sprintf("Signed in to ChatGPT (account %s).", creds.AccountID)
	if !creds.Expiry.IsZero() {
		line += fmt.Sprintf(" Access token expires %s.", creds.Expiry.Format("2006-01-02 15:04:05 MST"))
	}
	return line, true
}
