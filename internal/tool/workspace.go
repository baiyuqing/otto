package tool

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"syscall"
)

type Workspace struct {
	root        string
	lexicalRoot string
	rootFS      *os.Root
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
	rootFS, err := os.OpenRoot(resolved)
	if err != nil {
		return nil, err
	}
	return &Workspace{root: resolved, lexicalRoot: clean, rootFS: rootFS}, nil
}

// Close releases the directory handle used to keep file tools in this workspace.
func (w *Workspace) Close() error { return w.rootFS.Close() }

// Open opens an existing workspace file through the workspace directory handle.
// The caller must close the returned file.
func (w *Workspace) Open(path string) (*os.File, error) {
	rel, err := w.existingRelative(path)
	if err != nil {
		return nil, err
	}
	return w.openRelative(rel)
}

func (w *Workspace) openRelative(path string) (*os.File, error) {
	return w.rootFS.OpenFile(path, os.O_RDONLY|syscall.O_NONBLOCK, 0)
}

func (w *Workspace) existingRelative(path string) (string, error) {
	rel, err := w.directRelative(path)
	if err != nil {
		return "", err
	}
	file, openErr := w.openRelative(rel)
	if openErr == nil {
		_ = file.Close()
		if hasParentTraversal(rel) {
			return w.canonicalExistingRelative(path)
		}
		hasSymlink, err := w.pathHasSymlink(rel)
		if err != nil {
			return "", err
		}
		if hasSymlink {
			return w.canonicalExistingRelative(path)
		}
		return filepath.Clean(rel), nil
	}
	if filepath.IsAbs(path) {
		return "", openErr
	}
	// os.Root rejects absolute symlinks, including ones pointing back into the
	// workspace. Convert a verified internal target to a Root-relative name.
	canonical, canonicalErr := w.canonicalExistingRelative(path)
	if canonicalErr != nil {
		return "", canonicalErr
	}
	if canonical != rel {
		file, err := w.openRelative(canonical)
		if err == nil {
			_ = file.Close()
			return canonical, nil
		}
		return "", err
	}
	return "", openErr
}

func (w *Workspace) ResolveExisting(path string) (string, error) {
	return w.canonicalExisting(path)
}

func (w *Workspace) ResolveForWrite(path string) (string, error) {
	return w.canonicalForWrite(path)
}

func (w *Workspace) directRelative(path string) (string, error) {
	if path == "" || path == "." {
		return ".", nil
	}
	if filepath.IsAbs(path) {
		return w.canonicalExistingRelative(path)
	}
	return path, nil
}

// writeRelative returns a Root-relative name without resolving the final path
// through ordinary filesystem I/O. It only falls back to canonical conversion
// for old absolute symlinks that os.Root intentionally rejects.
func (w *Workspace) writeRelative(path string) (string, error) {
	if filepath.IsAbs(path) || hasParentTraversal(path) {
		canonical, err := w.canonicalForWrite(path)
		if err != nil {
			return "", err
		}
		return filepath.Rel(w.root, canonical)
	}
	rel, err := w.directRelative(path)
	if err != nil {
		return "", err
	}
	for ancestor := filepath.Dir(rel); ancestor != "."; ancestor = filepath.Dir(ancestor) {
		file, openErr := w.openRelative(ancestor)
		if openErr == nil {
			_ = file.Close()
			break
		}
		if os.IsNotExist(openErr) {
			continue
		}
		canonical, canonicalErr := w.canonicalForWrite(path)
		if canonicalErr != nil {
			return "", canonicalErr
		}
		return filepath.Rel(w.root, canonical)
	}
	return w.finalWriteRelative(rel, path)
}

func (w *Workspace) pathHasSymlink(path string) (bool, error) {
	current := ""
	for _, part := range strings.Split(filepath.ToSlash(path), "/") {
		if part == "" || part == "." {
			continue
		}
		current = filepath.Join(current, part)
		info, err := w.rootFS.Lstat(current)
		if err != nil {
			return false, err
		}
		if info.Mode()&os.ModeSymlink != 0 {
			return true, nil
		}
	}
	return false, nil
}

func (w *Workspace) finalWriteRelative(path, original string) (string, error) {
	for range 40 {
		info, err := w.rootFS.Lstat(path)
		if os.IsNotExist(err) {
			return path, nil
		}
		if err != nil || info.Mode()&os.ModeSymlink == 0 {
			if err != nil {
				return "", err
			}
			return path, nil
		}
		target, err := w.rootFS.Readlink(path)
		if err != nil {
			return "", err
		}
		if filepath.IsAbs(target) {
			canonical, err := w.canonicalForWrite(original)
			if err != nil {
				return "", err
			}
			return filepath.Rel(w.root, canonical)
		}
		parent := filepath.Dir(path)
		if parent == "." {
			path = target
		} else {
			path = parent + string(filepath.Separator) + target
		}
	}
	return "", fmt.Errorf("too many symbolic links: %s", original)
}

func (w *Workspace) canonicalExistingRelative(path string) (string, error) {
	resolved, err := w.canonicalExisting(path)
	if err != nil {
		return "", err
	}
	return filepath.Rel(w.root, resolved)
}

func (w *Workspace) canonicalExisting(path string) (string, error) {
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

func (w *Workspace) canonicalForWrite(path string) (string, error) {
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
