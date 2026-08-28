package session

import (
	"encoding/json"
	"errors"
)

const (
	PiSessionVersion     = 3
	maxSessionEntryBytes = 16 << 20
	maxSessionFileBytes  = 256 << 20
)

var (
	ErrUnsupportedSessionFormat  = errors.New("unsupported session format")
	ErrInvalidSession            = errors.New("invalid session")
	ErrSessionEntryTooLarge      = errors.New("session entry too large")
	ErrSessionFileTooLarge       = errors.New("session file too large")
	ErrUnsupportedSessionContent = errors.New("unsupported session content")
)

type piFile struct {
	Header  piHeader
	Entries []piEntry
}

type piHeader struct {
	Type          string          `json:"type"`
	Version       int             `json:"version"`
	ID            string          `json:"id"`
	Timestamp     string          `json:"timestamp"`
	CWD           string          `json:"cwd"`
	ParentSession *string         `json:"parentSession,omitempty"`
	Raw           json.RawMessage `json:"-"`
}

type piEntryBase struct {
	Type      string  `json:"type"`
	ID        string  `json:"id"`
	ParentID  *string `json:"parentId"`
	Timestamp string  `json:"timestamp"`
}

type piEntry struct {
	piEntryBase
	Raw                 json.RawMessage        `json:"-"`
	Message             *piMessage             `json:"-"`
	ModelChange         *piModelChange         `json:"-"`
	ThinkingLevelChange *piThinkingLevelChange `json:"-"`
	Compaction          *piCompaction          `json:"-"`
	BranchSummary       *piBranchSummary       `json:"-"`
	Custom              *piCustom              `json:"-"`
	CustomMessage       *piCustomMessage       `json:"-"`
	Label               *piLabel               `json:"-"`
	SessionInfo         *piSessionInfo         `json:"-"`
}

type piMessage struct {
	Role               string          `json:"role"`
	Content            json.RawMessage `json:"content,omitempty"`
	API                string          `json:"api,omitempty"`
	Provider           string          `json:"provider,omitempty"`
	Model              string          `json:"model,omitempty"`
	ResponseModel      string          `json:"responseModel,omitempty"`
	ResponseID         string          `json:"responseId,omitempty"`
	Diagnostics        json.RawMessage `json:"diagnostics,omitempty"`
	Usage              *piUsage        `json:"usage,omitempty"`
	StopReason         string          `json:"stopReason,omitempty"`
	Deferred           json.RawMessage `json:"deferred,omitempty"`
	ErrorMessage       string          `json:"errorMessage,omitempty"`
	RawStopReason      string          `json:"rawStopReason,omitempty"`
	EndTurn            *bool           `json:"endTurn,omitempty"`
	ToolCallID         string          `json:"toolCallId,omitempty"`
	ToolName           string          `json:"toolName,omitempty"`
	Details            json.RawMessage `json:"details,omitempty"`
	AddedToolNames     []string        `json:"addedToolNames,omitempty"`
	IsError            *bool           `json:"isError,omitempty"`
	Command            string          `json:"command,omitempty"`
	Output             string          `json:"output,omitempty"`
	ExitCode           *int            `json:"exitCode,omitempty"`
	Cancelled          *bool           `json:"cancelled,omitempty"`
	Truncated          *bool           `json:"truncated,omitempty"`
	FullOutputPath     string          `json:"fullOutputPath,omitempty"`
	ExcludeFromContext *bool           `json:"excludeFromContext,omitempty"`
	CustomType         string          `json:"customType,omitempty"`
	Display            *bool           `json:"display,omitempty"`
	Summary            string          `json:"summary,omitempty"`
	FromID             string          `json:"fromId,omitempty"`
	TokensBefore       *int64          `json:"tokensBefore,omitempty"`
	Timestamp          int64           `json:"timestamp"`

	ContentText   *string          `json:"-"`
	ContentBlocks []piContentBlock `json:"-"`
}

type piContentBlock struct {
	Type              string          `json:"type"`
	Text              string          `json:"text,omitempty"`
	TextSignature     string          `json:"textSignature,omitempty"`
	Data              string          `json:"data,omitempty"`
	MIMEType          string          `json:"mimeType,omitempty"`
	Thinking          string          `json:"thinking,omitempty"`
	ThinkingSignature string          `json:"thinkingSignature,omitempty"`
	Redacted          *bool           `json:"redacted,omitempty"`
	ID                string          `json:"id,omitempty"`
	Name              string          `json:"name,omitempty"`
	Arguments         json.RawMessage `json:"arguments,omitempty"`
	ThoughtSignature  string          `json:"thoughtSignature,omitempty"`
	Namespace         string          `json:"namespace,omitempty"`
	Raw               json.RawMessage `json:"-"`
}

type piUsage struct {
	Input        int64  `json:"input"`
	Output       int64  `json:"output"`
	CacheRead    int64  `json:"cacheRead"`
	CacheWrite   int64  `json:"cacheWrite"`
	CacheWrite1H *int64 `json:"cacheWrite1h,omitempty"`
	Reasoning    *int64 `json:"reasoning,omitempty"`
	TotalTokens  int64  `json:"totalTokens"`
	Cost         piCost `json:"cost"`
}

type piCost struct {
	Input      float64 `json:"input"`
	Output     float64 `json:"output"`
	CacheRead  float64 `json:"cacheRead"`
	CacheWrite float64 `json:"cacheWrite"`
	Total      float64 `json:"total"`
}

type piModelChange struct {
	Provider string `json:"provider"`
	ModelID  string `json:"modelId"`
}

type piThinkingLevelChange struct {
	ThinkingLevel string `json:"thinkingLevel"`
}

type piCompaction struct {
	Summary          string          `json:"summary"`
	FirstKeptEntryID *string         `json:"firstKeptEntryId,omitempty"`
	TokensBefore     int64           `json:"tokensBefore"`
	Details          json.RawMessage `json:"details,omitempty"`
	Usage            *piUsage        `json:"usage,omitempty"`
	FromHook         *bool           `json:"fromHook,omitempty"`
	RetainedTail     []piMessage     `json:"retainedTail,omitempty"`
}

type piBranchSummary struct {
	FromID   string          `json:"fromId"`
	Summary  string          `json:"summary"`
	Details  json.RawMessage `json:"details,omitempty"`
	Usage    *piUsage        `json:"usage,omitempty"`
	FromHook *bool           `json:"fromHook,omitempty"`
}

type piCustom struct {
	CustomType string          `json:"customType"`
	Data       json.RawMessage `json:"data,omitempty"`
}

type piCustomMessage struct {
	CustomType    string           `json:"customType"`
	Content       json.RawMessage  `json:"content"`
	Details       json.RawMessage  `json:"details,omitempty"`
	Display       bool             `json:"display"`
	ContentText   *string          `json:"-"`
	ContentBlocks []piContentBlock `json:"-"`
}

type piLabel struct {
	TargetID string  `json:"targetId"`
	Label    *string `json:"label,omitempty"`
}

type piSessionInfo struct {
	Name *string `json:"name,omitempty"`
}
