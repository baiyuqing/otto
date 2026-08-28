package tool

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

type Workspace struct {
	root        string
	lexicalRoot string
}

func NewWorkspace(root string) (*Workspace, error) {
	abs, err := filepath.Abs(root)
	if err != nil {
		return nil, err
	}
	clean := filepath.Clean(abs)
	resolved, err := filepath.EvalSymlinks(clean)
	if err != nil {
		return nil, err
	}
	return &Workspace{root: resolved, lexicalRoot: clean}, nil
}

func (w *Workspace) ResolveExisting(path string) (string, error) {
	candidate := w.candidatePath(path)
	resolved, err := filepath.EvalSymlinks(candidate)
	if err != nil {
		return "", err
	}
	if err := w.ensureInside(resolved); err != nil {
		return "", err
	}
	return resolved, nil
}

func (w *Workspace) ResolveForWrite(path string) (string, error) {
	candidate := w.candidatePath(path)
	ancestor, suffix, err := w.findExistingAncestor(candidate)
	if err != nil {
		return "", err
	}
	resolvedAncestor, err := filepath.EvalSymlinks(ancestor)
	if err != nil {
		return "", err
	}
	if err := w.ensureInside(resolvedAncestor); err != nil {
		return "", err
	}
	if suffix == "" {
		return resolvedAncestor, nil
	}
	if hasParentTraversal(suffix) {
		return "", fmt.Errorf("path escapes workspace: %s", path)
	}
	return filepath.Join(resolvedAncestor, suffix), nil
}

func (w *Workspace) candidatePath(path string) string {
	if path == "" {
		return w.root
	}
	if filepath.IsAbs(path) {
		return path
	}
	return w.root + string(os.PathSeparator) + path
}

func (w *Workspace) findExistingAncestor(candidate string) (string, string, error) {
	ancestor := candidate
	for {
		if _, err := os.Lstat(ancestor); err == nil {
			suffix := strings.TrimPrefix(candidate, ancestor)
			suffix = strings.TrimPrefix(suffix, string(os.PathSeparator))
			return ancestor, suffix, nil
		} else if !os.IsNotExist(err) {
			return "", "", err
		}

		parent := filepath.Dir(ancestor)
		if parent == ancestor {
			return "", "", fmt.Errorf("path escapes workspace: %s", candidate)
		}
		ancestor = parent
	}
}

func (w *Workspace) ensureInside(candidate string) error {
	rel, err := filepath.Rel(w.root, candidate)
	if err != nil {
		return err
	}
	if rel == ".." || strings.HasPrefix(rel, ".."+string(os.PathSeparator)) {
		return fmt.Errorf("path escapes workspace: %s", candidate)
	}
	return nil
}

func hasParentTraversal(path string) bool {
	for _, part := range strings.Split(filepath.ToSlash(path), "/") {
		if part == ".." {
			return true
		}
	}
	return false
}
