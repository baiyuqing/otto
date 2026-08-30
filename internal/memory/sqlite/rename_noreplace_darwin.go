//go:build darwin

package sqlite

import "golang.org/x/sys/unix"

func renameDirectoryNoReplace(oldDirectory int, oldName string, newDirectory int, newName string) error {
	return unix.RenameatxNp(oldDirectory, oldName, newDirectory, newName, unix.RENAME_EXCL)
}
