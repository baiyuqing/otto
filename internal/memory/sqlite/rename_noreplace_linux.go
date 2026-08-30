//go:build linux

package sqlite

import "golang.org/x/sys/unix"

func renameDirectoryNoReplace(oldDirectory int, oldName string, newDirectory int, newName string) error {
	return unix.Renameat2(oldDirectory, oldName, newDirectory, newName, unix.RENAME_NOREPLACE)
}
