package memory

import (
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"path/filepath"
	"strings"
	"testing"
)

func TestWorkspaceScopeUsesStableOverrideOrPathDigest(t *testing.T) {
	workspace := t.TempDir()
	physical, err := filepath.EvalSymlinks(workspace)
	if err != nil {
		t.Fatal(err)
	}
	got, err := NewWorkspaceScope(workspace, "")
	if err != nil {
		t.Fatal(err)
	}
	sum := sha256.Sum256([]byte(physical))
	want := Scope{Namespace: NamespaceWorkspace, ID: "sha256:" + hex.EncodeToString(sum[:])}
	if got != want {
		t.Fatalf("scope = %#v, want %#v", got, want)
	}

	got, err = NewWorkspaceScope(workspace, "stable-project")
	if err != nil {
		t.Fatal(err)
	}
	if want := (Scope{Namespace: NamespaceWorkspace, ID: "stable-project"}); got != want {
		t.Fatalf("override scope = %#v, want %#v", got, want)
	}
}

func TestWorkspaceScopeOverrideDoesNotInspectPath(t *testing.T) {
	got, err := NewWorkspaceScope("/raw/path/that/must/not/be/inspected", "stable-project")
	if err != nil {
		t.Fatal(err)
	}
	if got != (Scope{Namespace: NamespaceWorkspace, ID: "stable-project"}) {
		t.Fatalf("scope = %#v", got)
	}
}

func TestScopeConstructorsValidateWithoutLeakingInput(t *testing.T) {
	user, err := NewUserScope("installation-1")
	if err != nil {
		t.Fatal(err)
	}
	if user != (Scope{Namespace: NamespaceUser, ID: "installation-1"}) {
		t.Fatalf("user scope = %#v", user)
	}

	for name, call := range map[string]func() (Scope, error){
		"empty user":      func() (Scope, error) { return NewUserScope("") },
		"unsafe user":     func() (Scope, error) { return NewUserScope("private installation id") },
		"empty workspace": func() (Scope, error) { return NewWorkspaceScope("", "") },
		"unsafe override": func() (Scope, error) { return NewWorkspaceScope("/unused", "private project id") },
	} {
		t.Run(name, func(t *testing.T) {
			_, err := call()
			if !errors.Is(err, ErrInvalidRequest) {
				t.Fatalf("error = %v, want ErrInvalidRequest", err)
			}
			for _, content := range []string{"private installation id", "private project id", "/unused"} {
				if strings.Contains(err.Error(), content) {
					t.Fatalf("error leaks input: %q", err)
				}
			}
		})
	}
}
