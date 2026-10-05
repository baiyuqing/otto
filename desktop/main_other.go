//go:build !darwin

package main

import (
	"fmt"
	"os"
)

// Keep the module discoverable by MyGo's cross-platform build tooling;
// the actual desktop application and its native UI remain macOS-only.
func main() {
	fmt.Fprintln(os.Stderr, "Otto desktop is supported on macOS only.")
	os.Exit(1)
}
