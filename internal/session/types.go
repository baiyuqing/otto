package session

import (
	"context"
	"errors"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

const CurrentVersion = PiSessionVersion

var ErrFatalPersistence = errors.New("fatal session persistence failure")

type fatalPersistenceError struct {
	cause error
}

func (err *fatalPersistenceError) Error() string {
	return ErrFatalPersistence.Error() + ": " + err.cause.Error()
}

func (err *fatalPersistenceError) Unwrap() error {
	return err.cause
}

func (err *fatalPersistenceError) Is(target error) bool {
	return target == ErrFatalPersistence
}

type RuntimeMetadata struct {
	Profile  string `json:"profile,omitempty"`
	Provider string `json:"provider"`
	Model    string `json:"model"`
}

type CompactionDetails struct {
	ReadFiles            []string `json:"readFiles,omitempty"`
	ModifiedFiles        []string `json:"modifiedFiles,omitempty"`
	OmittedReadFiles     int      `json:"omittedReadFiles,omitempty"`
	OmittedModifiedFiles int      `json:"omittedModifiedFiles,omitempty"`
}

type CompactionMetadata struct {
	ID                           string
	Summary                      string
	FirstKeptEntryID             string
	TokensBefore                 int
	Usage                        *model.Usage
	Details                      CompactionDetails
	RetainedTailOnly             bool
	FirstPostCheckpointMessageID string
}

type Header struct {
	Version   int       `json:"version"`
	ID        string    `json:"id"`
	Workspace string    `json:"workspace"`
	Provider  string    `json:"provider"`
	Profile   string    `json:"profile,omitempty"`
	Model     string    `json:"model"`
	CreatedAt time.Time `json:"created_at"`
}

type Warning struct {
	Message string
}

type SessionInfo struct {
	Path         string
	ID           string
	CWD          string
	Name         string
	Created      time.Time
	Modified     time.Time
	MessageCount int
	LastUserText string
	Profile      string
	Provider     string
	Model        string
	Current      bool
}

type ListResult struct {
	Sessions []SessionInfo
	Skipped  int
}

type UsageProvider interface {
	AggregateUsage() (model.Usage, bool)
}

type RuntimeUpdater interface {
	UpdateRuntime(context.Context, RuntimeMetadata) error
}

type Session interface {
	Header() Header
	Messages() []model.Message
	Append(context.Context, model.Message) error
	Path() string
	Close() error
}
