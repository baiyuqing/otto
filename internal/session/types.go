package session

import (
	"context"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

const currentVersion = 1

const (
	recordTypeHeader  = "header"
	recordTypeMessage = "message"
)

type Header struct {
	Version   int       `json:"version"`
	ID        string    `json:"id"`
	Workspace string    `json:"workspace"`
	Provider  string    `json:"provider"`
	Profile   string    `json:"profile,omitempty"`
	Model     string    `json:"model"`
	CreatedAt time.Time `json:"created_at"`
}

type Record struct {
	Type    string         `json:"type"`
	Header  *Header        `json:"header,omitempty"`
	Message *model.Message `json:"message,omitempty"`
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
