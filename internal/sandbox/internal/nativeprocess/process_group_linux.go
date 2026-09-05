//go:build linux

package nativeprocess

func processGroupTerminated(int) bool { return false }
