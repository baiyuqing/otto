package session

import (
	"encoding/json"
	"time"

	"github.com/baiyuqing/otto/internal/model"
)

type persistedRecord struct {
	Type    string            `json:"type"`
	Header  *persistedHeader  `json:"header,omitempty"`
	Message *persistedMessage `json:"message,omitempty"`
}

type persistedHeader struct {
	Version   int       `json:"version"`
	ID        string    `json:"id"`
	Workspace string    `json:"workspace"`
	Provider  string    `json:"provider"`
	Profile   string    `json:"profile,omitempty"`
	Model     string    `json:"model"`
	CreatedAt time.Time `json:"created_at"`
}

type persistedMessage struct {
	ID           string             `json:"id"`
	Role         model.Role         `json:"role"`
	Blocks       []persistedBlock   `json:"blocks"`
	CreatedAt    time.Time          `json:"created_at"`
	FinishReason model.FinishReason `json:"finish_reason,omitempty"`
	Usage        *persistedUsage    `json:"usage,omitempty"`
}

type persistedBlock struct {
	Type       model.BlockType `json:"type"`
	Text       string          `json:"text,omitempty"`
	ToolCallID string          `json:"tool_call_id,omitempty"`
	ToolName   string          `json:"tool_name,omitempty"`
	Arguments  json.RawMessage `json:"arguments,omitempty"`
	IsError    bool            `json:"is_error,omitempty"`
}

type persistedUsage struct {
	InputTokens  int `json:"input_tokens"`
	OutputTokens int `json:"output_tokens"`
}

func newPersistedHeader(header Header) *persistedHeader {
	return &persistedHeader{
		Version:   header.Version,
		ID:        header.ID,
		Workspace: header.Workspace,
		Provider:  header.Provider,
		Profile:   header.Profile,
		Model:     header.Model,
		CreatedAt: header.CreatedAt,
	}
}

func (header persistedHeader) sessionHeader() Header {
	return Header(header)
}

func newPersistedMessage(message model.Message) *persistedMessage {
	persisted := &persistedMessage{
		ID:           message.ID,
		Role:         message.Role,
		CreatedAt:    message.CreatedAt,
		FinishReason: message.FinishReason,
	}
	if message.Blocks != nil {
		persisted.Blocks = make([]persistedBlock, len(message.Blocks))
		for i, block := range message.Blocks {
			persisted.Blocks[i] = persistedBlock{
				Type:       block.Type,
				Text:       block.Text,
				ToolCallID: block.ToolCallID,
				ToolName:   block.ToolName,
				Arguments:  append(json.RawMessage(nil), block.Arguments...),
				IsError:    block.IsError,
			}
		}
	}
	if message.Usage != nil {
		persisted.Usage = &persistedUsage{InputTokens: message.Usage.InputTokens, OutputTokens: message.Usage.OutputTokens}
	}
	return persisted
}

func (message persistedMessage) modelMessage() model.Message {
	converted := model.Message{
		ID:           message.ID,
		Role:         message.Role,
		CreatedAt:    message.CreatedAt,
		FinishReason: message.FinishReason,
	}
	if message.Blocks != nil {
		converted.Blocks = make([]model.Block, len(message.Blocks))
		for i, block := range message.Blocks {
			converted.Blocks[i] = model.Block{
				Type:       block.Type,
				Text:       block.Text,
				ToolCallID: block.ToolCallID,
				ToolName:   block.ToolName,
				Arguments:  append(json.RawMessage(nil), block.Arguments...),
				IsError:    block.IsError,
			}
		}
	}
	if message.Usage != nil {
		converted.Usage = &model.Usage{InputTokens: message.Usage.InputTokens, OutputTokens: message.Usage.OutputTokens}
	}
	return converted
}

func newPersistedHeaderRecord(header Header) persistedRecord {
	return persistedRecord{Type: recordTypeHeader, Header: newPersistedHeader(header)}
}

func newPersistedMessageRecord(message model.Message) persistedRecord {
	return persistedRecord{Type: recordTypeMessage, Message: newPersistedMessage(message)}
}
