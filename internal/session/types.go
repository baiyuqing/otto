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

type Session interface {
	Header() Header
	Messages() []model.Message
	Append(context.Context, model.Message) error
	Path() string
	Close() error
}
