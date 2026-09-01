//go:build !darwin

package seatbelt

import (
	"context"

	"github.com/baiyuqing/otto/internal/sandbox"
)

const ID sandbox.DriverID = "seatbelt"

type Options struct {
	Workspace   string
	Shell       string
	Home        string
	CacheBase   string
	HostEntries []string
	ReadPaths   []string
	Network     sandbox.NetworkMode
}

type Driver struct{}

func Open(context.Context, Options) (*Driver, error) {
	return nil, &sandbox.UnavailableError{Reason: sandbox.ReasonUnsupportedPlatform}
}

func (*Driver) ID() sandbox.DriverID {
	return ID
}

func (*Driver) PrivateDirectories() sandbox.PrivateDirectories {
	return sandbox.PrivateDirectories{}
}

func (*Driver) Capabilities() sandbox.Capabilities {
	return sandbox.Capabilities{}
}

func (*Driver) Execute(context.Context, sandbox.Request, sandbox.Streams) (sandbox.ExitStatus, error) {
	return sandbox.ExitStatus{}, &sandbox.UnavailableError{Reason: sandbox.ReasonUnsupportedPlatform}
}

func (*Driver) Close() error {
	return nil
}

var _ sandbox.Driver = (*Driver)(nil)
