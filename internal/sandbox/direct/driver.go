package direct

import (
	"context"
	"reflect"

	"github.com/baiyuqing/otto/internal/sandbox"
	"github.com/baiyuqing/otto/internal/sandbox/internal/nativeprocess"
)

const ID sandbox.DriverID = "direct"

type driver struct {
	processes *nativeprocess.Manager
}

func New() sandbox.Driver {
	return &driver{processes: nativeprocess.New()}
}

func (d *driver) ID() sandbox.DriverID {
	return ID
}

func (d *driver) Capabilities() sandbox.Capabilities {
	return sandbox.Capabilities{NetworkAllow: true}
}

func (d *driver) Execute(ctx context.Context, request sandbox.Request, streams sandbox.Streams) (sandbox.ExitStatus, error) {
	if isNil(ctx) || len(request.Argv) == 0 || isNil(streams.Stdout) || isNil(streams.Stderr) {
		return sandbox.ExitStatus{}, sandbox.ErrInvalidRequest
	}
	result, err := d.processes.Run(ctx, nativeprocess.Spec{
		Path:        request.Argv[0],
		Args:        append([]string(nil), request.Argv[1:]...),
		Directory:   request.Dir,
		Environment: append([]string{}, request.Env...),
		Stdout:      streams.Stdout,
		Stderr:      streams.Stderr,
	})
	return sandbox.ExitStatus{
		Code:     result.Code,
		Signaled: result.Signaled,
		Signal:   result.Signal,
	}, err
}

func (d *driver) Close() error {
	return d.processes.Close()
}

func isNil(value any) bool {
	if value == nil {
		return true
	}
	reflected := reflect.ValueOf(value)
	switch reflected.Kind() {
	case reflect.Chan, reflect.Func, reflect.Interface, reflect.Map, reflect.Pointer, reflect.Slice:
		return reflected.IsNil()
	default:
		return false
	}
}

var _ sandbox.Driver = (*driver)(nil)
