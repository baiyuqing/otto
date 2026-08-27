package session

import (
	"bytes"
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

func TestPrepareDoesNotMutateBeforeActivationAndTransfersExactFileOwnership(t *testing.T) {
	path := createSessionWithDanglingCall(t)
	withoutFinalLF := bytes.TrimSuffix(readFile(t, path), []byte{'\n'})
	if err := os.WriteFile(path, withoutFinalLF, 0o600); err != nil {
		t.Fatal(err)
	}

	prepared, err := Prepare(context.Background(), path)
	if err != nil {
		t.Fatal(err)
	}
	if got := prepared.Info(); got.Path != path || got.ID == "" || got.Provider != "openai-compatible" || got.Model == "" {
		_ = prepared.Close()
		t.Fatalf("Info() = %#v", got)
	}
	if afterPrepare := readFile(t, path); !bytes.Equal(afterPrepare, withoutFinalLF) {
		_ = prepared.Close()
		t.Fatal("Prepare() mutated repairable session state")
	}

	opened, warnings, err := prepared.Activate(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(warnings) != 2 {
		_ = opened.Close()
		t.Fatalf("Activate() warnings = %#v, want delimiter and dangling-call repairs", warnings)
	}
	if err := prepared.Close(); err != nil {
		_ = opened.Close()
		t.Fatalf("Close() after activation = %v", err)
	}
	message := model.Message{Role: model.RoleUser, Blocks: []model.Block{{Type: model.BlockText, Text: "owned by store"}}, CreatedAt: time.Now().UTC()}
	if err := opened.Append(context.Background(), message); err != nil {
		_ = opened.Close()
		t.Fatalf("Append() after Prepared.Close() = %v; descriptor was not transferred", err)
	}
	if err := opened.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestPreparedActivateRejectsAtomicPathReplacementAndSymlinkWithoutMutation(t *testing.T) {
	for _, replacementKind := range []string{"regular file", "symlink"} {
		t.Run(replacementKind, func(t *testing.T) {
			root := t.TempDir()
			workspace := t.TempDir()
			original := createPreparedTestSession(t, root, workspace, "original")
			originalAlias := filepath.Join(t.TempDir(), "pinned-original.jsonl")
			if err := os.Link(original, originalAlias); err != nil {
				t.Fatal(err)
			}
			prepared, err := Prepare(context.Background(), original)
			if err != nil {
				t.Fatal(err)
			}
			originalBefore := readFile(t, originalAlias)

			replacement := createPreparedTestSession(t, root, workspace, "replacement")
			replacementBefore := readFile(t, replacement)
			replacementCurrentPath := replacement
			switch replacementKind {
			case "regular file":
				if err := os.Rename(replacement, original); err != nil {
					t.Fatal(err)
				}
				replacementCurrentPath = original
			case "symlink":
				if err := os.Remove(original); err != nil {
					t.Fatal(err)
				}
				if err := os.Symlink(replacement, original); err != nil {
					t.Fatal(err)
				}
			}

			opened, _, activateErr := prepared.Activate(context.Background())
			if opened != nil {
				_ = opened.Close()
				t.Fatal("Activate() returned a store for a replaced path")
			}
			if !errors.Is(activateErr, ErrInvalidSession) || !strings.Contains(activateErr.Error(), "identity changed") {
				t.Fatalf("Activate() error = %v, want identity change", activateErr)
			}
			if err := prepared.Close(); err != nil {
				t.Fatalf("Close() after failed activation = %v", err)
			}
			if got := readFile(t, originalAlias); !bytes.Equal(got, originalBefore) {
				t.Fatal("failed activation mutated the pinned original")
			}
			if got := readFile(t, replacementCurrentPath); !bytes.Equal(got, replacementBefore) {
				t.Fatal("failed activation mutated the replacement")
			}
		})
	}
}

func TestPreparedActivateRejectsMetadataIdentityMismatchWithoutMutation(t *testing.T) {
	root := t.TempDir()
	workspace := t.TempDir()
	path := createPreparedTestSession(t, root, workspace, "prepared")
	prepared, err := Prepare(context.Background(), path)
	if err != nil {
		t.Fatal(err)
	}

	replacement := createPreparedTestSession(t, root, workspace, "changed-in-place")
	changed := readFile(t, replacement)
	if err := os.WriteFile(path, changed, 0o600); err != nil {
		t.Fatal(err)
	}

	opened, _, activateErr := prepared.Activate(context.Background())
	if opened != nil {
		_ = opened.Close()
		t.Fatal("Activate() returned a store for changed prepared metadata")
	}
	if !errors.Is(activateErr, ErrInvalidSession) || !strings.Contains(activateErr.Error(), "metadata changed") {
		t.Fatalf("Activate() error = %v, want metadata change", activateErr)
	}
	if got := readFile(t, path); !bytes.Equal(got, changed) {
		t.Fatal("metadata mismatch failure mutated the changed file")
	}
}

func TestPreparedCloseAbandonsDescriptorAndIsIdempotent(t *testing.T) {
	path := createPreparedTestSession(t, t.TempDir(), t.TempDir(), "abandoned")
	prepared, err := Prepare(context.Background(), path)
	if err != nil {
		t.Fatal(err)
	}
	if err := prepared.Close(); err != nil {
		t.Fatal(err)
	}
	if err := prepared.Close(); err != nil {
		t.Fatalf("second Close() = %v", err)
	}
	opened, _, err := prepared.Activate(context.Background())
	if opened != nil {
		_ = opened.Close()
		t.Fatal("Activate() returned a store after Close()")
	}
	if err == nil || !strings.Contains(err.Error(), "no longer available") {
		t.Fatalf("Activate() error = %v, want unavailable handle", err)
	}
}

func TestPrepareRejectsSymlink(t *testing.T) {
	path := createPreparedTestSession(t, t.TempDir(), t.TempDir(), "target")
	link := filepath.Join(t.TempDir(), "linked.jsonl")
	if err := os.Symlink(path, link); err != nil {
		t.Fatal(err)
	}
	prepared, err := Prepare(context.Background(), link)
	if prepared != nil {
		_ = prepared.Close()
		t.Fatal("Prepare() returned a handle for a symlink")
	}
	if !errors.Is(err, ErrInvalidSession) {
		t.Fatalf("Prepare() error = %v, want ErrInvalidSession", err)
	}
}

func createPreparedTestSession(t *testing.T, root, workspace, id string) string {
	t.Helper()
	store, err := Create(root, Header{
		Version: CurrentVersion, ID: id, Workspace: workspace, Provider: "openai-compatible",
		Profile: "default", Model: id + "-model", CreatedAt: time.Now().UTC(),
	})
	if err != nil {
		t.Fatal(err)
	}
	path := store.Path()
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	return path
}
