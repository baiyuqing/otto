package sandbox

import (
	"context"
	"io"
	"slices"
)

type DriverID string

type DriverMode string

const (
	DriverAuto     DriverMode = "auto"
	DriverSeatbelt DriverMode = "seatbelt"
	DriverOff      DriverMode = "off"
)

type FilesystemMode uint8

const (
	FilesystemWorkspaceWrite FilesystemMode = iota + 1
	FilesystemUnconfined
)

type NetworkMode uint8

const (
	NetworkDeny NetworkMode = iota + 1
	NetworkAllow
)

type Policy struct {
	Filesystem FilesystemMode
	Network    NetworkMode
}

type Capabilities struct {
	ReadConfinement  bool
	WriteConfinement bool
	NetworkDeny      bool
	NetworkAllow     bool
	UnixSocketDeny   bool
}

type Driver interface {
	ID() DriverID
	Capabilities() Capabilities
	Execute(context.Context, Request, Streams) (ExitStatus, error)
	Close() error
}

type Request struct {
	Argv []string
	Dir  string
	Env  []string
}

func (r Request) Clone() Request {
	r.Argv = slices.Clone(r.Argv)
	r.Env = slices.Clone(r.Env)
	return r
}

type Streams struct {
	Stdout io.Writer
	Stderr io.Writer
}

type ExitStatus struct {
	Code     int
	Signaled bool
	Signal   string
}

type CommandExecutor interface {
	Execute(context.Context, Request, Streams) (ExitStatus, error)
}

type Settings struct {
	Driver    DriverMode
	Network   NetworkMode
	ReadPaths []string
	AllowEnv  []string
}

func (s Settings) Clone() Settings {
	s.ReadPaths = slices.Clone(s.ReadPaths)
	s.AllowEnv = slices.Clone(s.AllowEnv)
	return s
}

type PrivateDirectories struct {
	Root  string
	Home  string
	Temp  string
	Cache string
}
