//go:build !darwin && !linux

package sqlite

import (
	"context"
	"database/sql"

	"github.com/baiyuqing/otto/internal/memory"
)

const regularMode = uint32(0)

type syntheticMetadata struct {
	mode uint32
	uid  uint32
}

type inodeIdentity struct{}
type securePath struct{ canonicalPath string }

func currentUID() uint32                                          { return 0 }
func validateFileMetadata(syntheticMetadata, bool) error          { return memory.ErrUnsupported }
func openSecurePath(context.Context, string) (*securePath, error) { return nil, memory.ErrUnsupported }
func (*securePath) close() error                                  { return memory.ErrUnsupported }
func (*securePath) revalidate() error                             { return memory.ErrUnsupported }
func (*securePath) validateSidecarEntries() error                 { return memory.ErrUnsupported }
func (*securePath) sidecarIdentities() map[string]inodeIdentity   { return nil }
func snapshotProcessFDs() (map[inodeIdentity]int, error)          { return nil, memory.ErrUnsupported }
func proveSQLiteConnection(context.Context, *sql.Conn, map[inodeIdentity]int, *securePath) error {
	return memory.ErrUnsupported
}
func proveSQLiteSidecarsIfPresent(*securePath, map[inodeIdentity]int) error {
	return memory.ErrUnsupported
}
func proveSQLiteSidecars(*securePath, map[inodeIdentity]int, bool) error {
	return memory.ErrUnsupported
}
func proveRetainedSQLiteConnection(*securePath, map[inodeIdentity]int) error {
	return memory.ErrUnsupported
}
