package auth

import (
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestStatusLineNotSignedIn(t *testing.T) {
	path := filepath.Join(t.TempDir(), "chatgpt.json")
	line, signedIn := StatusLine(path)
	if signedIn {
		t.Fatalf("signedIn = true, want false")
	}
	if !strings.Contains(line, "Not signed in") {
		t.Fatalf("line = %q", line)
	}
}

func TestStatusLineSignedIn(t *testing.T) {
	path := filepath.Join(t.TempDir(), "chatgpt.json")
	creds := Credentials{AccountID: "acct-123", Expiry: time.Date(2030, 1, 2, 3, 4, 5, 0, time.UTC)}
	if err := creds.Save(path); err != nil {
		t.Fatal(err)
	}
	line, signedIn := StatusLine(path)
	if !signedIn {
		t.Fatalf("signedIn = false, want true")
	}
	if strings.Contains(line, "acct-123") || !strings.Contains(line, "Signed in to ChatGPT") || !strings.Contains(line, "expires") {
		t.Fatalf("line = %q", line)
	}
}
