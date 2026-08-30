package memory

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"path/filepath"
	"unicode/utf8"
)

func NewUserScope(installationID string) (Scope, error) {
	if !validOpaqueID(installationID, MaxScopeIDBytes) {
		return Scope{}, fmt.Errorf("%w: installation ID", ErrInvalidRequest)
	}
	return Scope{Namespace: NamespaceUser, ID: installationID}, nil
}

func NewWorkspaceScope(canonicalPath, stableOverride string) (Scope, error) {
	if stableOverride != "" {
		if !validOpaqueID(stableOverride, MaxScopeIDBytes) {
			return Scope{}, fmt.Errorf("%w: workspace scope override", ErrInvalidRequest)
		}
		return Scope{Namespace: NamespaceWorkspace, ID: stableOverride}, nil
	}
	if canonicalPath == "" || !utf8.ValidString(canonicalPath) || containsPathControl(canonicalPath) {
		return Scope{}, fmt.Errorf("%w: canonical workspace path", ErrInvalidRequest)
	}
	absolutePath, err := filepath.Abs(canonicalPath)
	if err != nil {
		return Scope{}, fmt.Errorf("%w: canonical workspace path", ErrInvalidRequest)
	}
	physicalPath, err := filepath.EvalSymlinks(absolutePath)
	if err != nil {
		return Scope{}, fmt.Errorf("%w: canonical workspace path", ErrInvalidRequest)
	}
	physicalPath, err = filepath.Abs(physicalPath)
	if err != nil {
		return Scope{}, fmt.Errorf("%w: canonical workspace path", ErrInvalidRequest)
	}
	digest := sha256.Sum256([]byte(physicalPath))
	return Scope{
		Namespace: NamespaceWorkspace,
		ID:        "sha256:" + hex.EncodeToString(digest[:]),
	}, nil
}

func containsPathControl(path string) bool {
	for _, r := range path {
		if r == 0 || (r >= 1 && r <= 31) || (r >= 127 && r <= 159) {
			return true
		}
	}
	return false
}
