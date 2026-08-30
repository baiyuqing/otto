package sqlite

import (
	"context"

	"github.com/baiyuqing/otto/internal/memory"
)

// Factory is an immutable SQLite component factory. Each successful Open owns
// one Store and one retained connection pool; the Retriever shares that Store.
type Factory struct {
	path    string
	options Options
}

// NewFactory copies path and options into a reusable Factory. Validation is
// deferred to Open so construction remains value-only and side-effect free.
func NewFactory(path string, options Options) Factory {
	return Factory{path: path, options: options}
}

func (factory Factory) Open(ctx context.Context) (memory.Components, error) {
	if err := ctx.Err(); err != nil {
		return memory.Components{}, err
	}
	if factory.path == "" {
		return memory.Components{}, memory.ErrInvalidRequest
	}
	store, err := Open(ctx, factory.path, factory.options)
	if err != nil {
		return memory.Components{}, err
	}
	return memory.Components{
		Store:       store,
		Retriever:   store,
		Maintenance: unsupportedMaintenance{},
		Capabilities: memory.Capabilities{
			LexicalSearch:       true,
			SemanticSearch:      false,
			OnlineBackup:        false,
			EncryptionAtRest:    false,
			ConcurrentProcesses: false,
		},
	}, nil
}

type unsupportedMaintenance struct{}

func (unsupportedMaintenance) Backup(ctx context.Context, _ memory.BackupRequest) (memory.BackupInfo, error) {
	if err := ctx.Err(); err != nil {
		return memory.BackupInfo{}, err
	}
	return memory.BackupInfo{}, memory.ErrUnsupported
}

func (unsupportedMaintenance) ListBackups(ctx context.Context) ([]memory.BackupInfo, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	return nil, memory.ErrUnsupported
}

func (unsupportedMaintenance) VerifyBackup(ctx context.Context, _ string) (memory.BackupInfo, error) {
	if err := ctx.Err(); err != nil {
		return memory.BackupInfo{}, err
	}
	return memory.BackupInfo{}, memory.ErrUnsupported
}

func (unsupportedMaintenance) Restore(ctx context.Context, _ memory.RestoreRequest) error {
	if err := ctx.Err(); err != nil {
		return err
	}
	return memory.ErrUnsupported
}

func (unsupportedMaintenance) PurgeForgotten(ctx context.Context, _ memory.PurgeForgottenRequest) (memory.PurgeForgottenResult, error) {
	if err := ctx.Err(); err != nil {
		return memory.PurgeForgottenResult{}, err
	}
	return memory.PurgeForgottenResult{}, memory.ErrUnsupported
}

var _ memory.Factory = Factory{}
var _ memory.Maintenance = unsupportedMaintenance{}
