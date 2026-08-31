package sandbox

import (
	"context"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"sync"
)

type Executor struct {
	driver       Driver
	id           DriverID
	capabilities Capabilities
	policy       Policy
	workspace    string

	mu        sync.Mutex
	closed    bool
	closeDone chan struct{}
	closeErr  error
}

func NewExecutor(driver Driver, policy Policy, workspace string) (*Executor, error) {
	if isNilInterface(driver) {
		return nil, &UnavailableError{Reason: ReasonRuntimeFailure}
	}

	id := driver.ID()
	if !validDriverID(id) {
		return nil, &UnavailableError{Reason: ReasonRuntimeFailure}
	}
	capabilities := driver.Capabilities()
	if !supportsPolicy(capabilities, policy) {
		return nil, ErrUnsupportedPolicy
	}
	canonicalWorkspace, err := canonicalDirectory(workspace)
	if err != nil {
		return nil, ErrInvalidRequest
	}

	return &Executor{
		driver:       driver,
		id:           id,
		capabilities: capabilities,
		policy:       policy,
		workspace:    canonicalWorkspace,
		closeDone:    make(chan struct{}),
	}, nil
}

func (e *Executor) Execute(ctx context.Context, request Request, streams Streams) (ExitStatus, error) {
	if isNilInterface(ctx) {
		return ExitStatus{}, ErrInvalidRequest
	}
	if err := ctx.Err(); err != nil {
		return ExitStatus{}, err
	}
	if e.isClosed() {
		return ExitStatus{}, ErrClosed
	}

	driverRequest, err := e.validatedRequest(request, streams)
	if err != nil {
		return ExitStatus{}, err
	}
	if err := ctx.Err(); err != nil {
		return ExitStatus{}, err
	}

	e.mu.Lock()
	if e.closed {
		e.mu.Unlock()
		return ExitStatus{}, ErrClosed
	}
	e.mu.Unlock()

	status, err := e.driver.Execute(ctx, driverRequest, streams)
	if status.Signaled {
		status.Code = -1
	}
	return status, err
}

func (e *Executor) Close() error {
	e.mu.Lock()
	if e.closed {
		done := e.closeDone
		e.mu.Unlock()
		<-done

		e.mu.Lock()
		err := e.closeErr
		e.mu.Unlock()
		return err
	}
	e.closed = true
	e.mu.Unlock()

	err := e.driver.Close()

	e.mu.Lock()
	e.closeErr = err
	close(e.closeDone)
	e.mu.Unlock()
	return err
}

func (e *Executor) ID() DriverID {
	return e.id
}

func (e *Executor) Capabilities() Capabilities {
	return e.capabilities
}

func (e *Executor) isClosed() bool {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.closed
}

func (e *Executor) validatedRequest(request Request, streams Streams) (Request, error) {
	if len(request.Argv) == 0 || isNilInterface(streams.Stdout) || isNilInterface(streams.Stderr) {
		return Request{}, ErrInvalidRequest
	}
	for _, arg := range request.Argv {
		if strings.IndexByte(arg, 0) >= 0 {
			return Request{}, ErrInvalidRequest
		}
	}
	if !e.validDirectory(request.Dir) || !validEnvironment(request.Env) {
		return Request{}, ErrInvalidRequest
	}
	return request.Clone(), nil
}

func (e *Executor) validDirectory(dir string) bool {
	if dir == "" || strings.IndexByte(dir, 0) >= 0 || !filepath.IsAbs(dir) || filepath.Clean(dir) != dir {
		return false
	}
	canonical, err := filepath.EvalSymlinks(dir)
	if err != nil || canonical != dir {
		return false
	}
	info, err := os.Stat(canonical)
	if err != nil || !info.IsDir() {
		return false
	}
	relative, err := filepath.Rel(e.workspace, canonical)
	if err != nil {
		return false
	}
	return relative == "." || (relative != ".." && !strings.HasPrefix(relative, ".."+string(filepath.Separator)))
}

func canonicalDirectory(path string) (string, error) {
	if path == "" || strings.IndexByte(path, 0) >= 0 {
		return "", ErrInvalidRequest
	}
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", ErrInvalidRequest
	}
	canonical, err := filepath.EvalSymlinks(absolute)
	if err != nil {
		return "", ErrInvalidRequest
	}
	canonical = filepath.Clean(canonical)
	info, err := os.Stat(canonical)
	if err != nil || !info.IsDir() {
		return "", ErrInvalidRequest
	}
	return canonical, nil
}

func validEnvironment(environment []string) bool {
	names := make(map[string]struct{}, len(environment))
	for _, entry := range environment {
		if strings.IndexByte(entry, 0) >= 0 {
			return false
		}
		name, _, found := strings.Cut(entry, "=")
		if !found || !validEnvironmentName(name) {
			return false
		}
		if _, duplicate := names[name]; duplicate {
			return false
		}
		names[name] = struct{}{}
	}
	return true
}

func validEnvironmentName(name string) bool {
	if name == "" || !(name[0] == '_' || name[0] >= 'A' && name[0] <= 'Z' || name[0] >= 'a' && name[0] <= 'z') {
		return false
	}
	for i := 1; i < len(name); i++ {
		character := name[i]
		if character != '_' && (character < 'A' || character > 'Z') && (character < 'a' || character > 'z') && (character < '0' || character > '9') {
			return false
		}
	}
	return true
}

func validDriverID(id DriverID) bool {
	if len(id) < 1 || len(id) > 32 {
		return false
	}
	for _, character := range id {
		if character != '-' && (character < 'a' || character > 'z') && (character < '0' || character > '9') {
			return false
		}
	}
	return true
}

func supportsPolicy(capabilities Capabilities, policy Policy) bool {
	switch policy.Filesystem {
	case FilesystemWorkspaceWrite:
		if !capabilities.ReadConfinement || !capabilities.WriteConfinement || !capabilities.UnixSocketDeny {
			return false
		}
		switch policy.Network {
		case NetworkDeny:
			return capabilities.NetworkDeny
		case NetworkAllow:
			return capabilities.NetworkAllow
		default:
			return false
		}
	case FilesystemUnconfined:
		return policy.Network == NetworkAllow && capabilities.NetworkAllow
	default:
		return false
	}
}

func isNilInterface(value any) bool {
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
