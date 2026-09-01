package openairesponses

import (
	"encoding/json"
	"strings"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/provider"
	"github.com/baiyuqing/otto/internal/safetext"
)

type requestRedactor struct {
	values []string
	marker string
}

func newRequestRedactor(accessToken, accountID string) (*requestRedactor, bool) {
	if strings.TrimSpace(accessToken) == "" || strings.TrimSpace(accountID) == "" {
		return nil, false
	}
	collector := safetext.NewSecretCollector()
	if !collector.Add(accessToken) || !collector.Add(accountID) {
		return nil, false
	}
	values := collector.Values()
	marker, ok := safetext.DynamicRedactionMarker(values)
	if !ok {
		return nil, false
	}
	return &requestRedactor{values: values, marker: marker}, true
}

func (r *requestRedactor) redactString(text string) string {
	if r == nil || text == "" {
		return text
	}
	text = safetext.CanonicalizeUTF8(text)
	for _, value := range r.values {
		text = strings.ReplaceAll(text, value, r.marker)
	}
	return text
}

func (r *requestRedactor) redactJSON(raw json.RawMessage) json.RawMessage {
	if r == nil {
		return append(json.RawMessage(nil), raw...)
	}
	canonical := json.RawMessage(safetext.CanonicalizeUTF8(string(raw)))
	if len(canonical) == 0 {
		return nil
	}
	var value any
	decoder := json.NewDecoder(strings.NewReader(string(canonical)))
	decoder.UseNumber()
	if err := decoder.Decode(&value); err != nil {
		return json.RawMessage("null")
	}
	value = r.redactJSONValue(value)
	encoded, err := json.Marshal(value)
	if err != nil {
		return json.RawMessage("null")
	}
	return json.RawMessage(encoded)
}

func (r *requestRedactor) redactJSONValue(value any) any {
	switch value := value.(type) {
	case string:
		return r.redactString(value)
	case []any:
		for index := range value {
			value[index] = r.redactJSONValue(value[index])
		}
		return value
	case map[string]any:
		redacted := make(map[string]any, len(value))
		for key, item := range value {
			redacted[r.redactString(key)] = r.redactJSONValue(item)
		}
		return redacted
	default:
		return value
	}
}

func (r *requestRedactor) redactResponse(response provider.Response) provider.Response {
	response.Message = r.redactMessage(response.Message)
	return response
}

func (r *requestRedactor) redactMessage(message model.Message) model.Message {
	message.ID = r.redactString(message.ID)
	message.ContextType = r.redactString(message.ContextType)
	if len(message.Blocks) == 0 {
		return message
	}
	message.Blocks = append([]model.Block(nil), message.Blocks...)
	for index := range message.Blocks {
		block := &message.Blocks[index]
		block.Text = r.redactString(block.Text)
		block.ToolCallID = r.redactString(block.ToolCallID)
		block.ToolName = r.redactString(block.ToolName)
		if len(block.Arguments) > 0 {
			block.Arguments = r.redactJSON(block.Arguments)
		}
	}
	return message
}

type streamEventRedactor struct {
	redactor *requestRedactor
	emit     func(provider.StreamEvent)
	text     *requestStreamRedactor
	tools    map[string]*toolEventState
	order    []string
}

type toolEventState struct {
	toolCallID string
	toolName   string
	arguments  *requestStreamRedactor
}

func (r *requestRedactor) wrapEmit(emit func(provider.StreamEvent)) *streamEventRedactor {
	return &streamEventRedactor{
		redactor: r,
		emit:     emit,
		text:     r.newStream(),
		tools:    make(map[string]*toolEventState),
	}
}

func (r *streamEventRedactor) Emit(event provider.StreamEvent) {
	if r == nil || r.emit == nil {
		return
	}
	redacted := provider.StreamEvent{Type: event.Type}
	redacted.ToolCallID = r.redactor.redactString(event.ToolCallID)
	redacted.ToolName = r.redactor.redactString(event.ToolName)
	switch event.Type {
	case provider.StreamTextDelta:
		redacted.Text = r.text.Write(event.Text)
	case provider.StreamToolCallDelta:
		if event.Arguments != "" {
			state := r.toolState(event.ToolCallID, event.ToolName)
			state.toolCallID = redacted.ToolCallID
			state.toolName = redacted.ToolName
			redacted.Arguments = state.arguments.Write(event.Arguments)
		}
	}
	if redacted.Text == "" && redacted.ToolCallID == "" && redacted.ToolName == "" && redacted.Arguments == "" {
		return
	}
	r.emit(redacted)
}

func (r *streamEventRedactor) Flush() {
	if r == nil || r.emit == nil {
		return
	}
	if text := r.text.Flush(); text != "" {
		r.emit(provider.StreamEvent{Type: provider.StreamTextDelta, Text: text})
	}
	for _, key := range r.order {
		state := r.tools[key]
		if state == nil {
			continue
		}
		if arguments := state.arguments.Flush(); arguments != "" {
			r.emit(provider.StreamEvent{Type: provider.StreamToolCallDelta, ToolCallID: state.toolCallID, ToolName: state.toolName, Arguments: arguments})
		}
	}
}

func (r *streamEventRedactor) toolState(toolCallID, toolName string) *toolEventState {
	key := toolCallID + "\x00" + toolName
	if state := r.tools[key]; state != nil {
		return state
	}
	state := &toolEventState{arguments: r.redactor.newStream()}
	r.tools[key] = state
	r.order = append(r.order, key)
	return state
}

type requestStreamRedactor struct {
	redactor        *requestRedactor
	pending         string
	invalidBoundary []byte
}

func (r *requestRedactor) newStream() *requestStreamRedactor {
	return &requestStreamRedactor{redactor: r}
}

func (s *requestStreamRedactor) Write(text string) string {
	if s == nil || text == "" {
		return ""
	}
	return s.writeNormalized(s.normalizeUTF8(text, false))
}

func (s *requestStreamRedactor) Flush() string {
	if s == nil {
		return ""
	}
	s.pending += s.normalizeUTF8("", true)
	pending := s.pending
	s.pending = ""
	s.invalidBoundary = nil
	return s.redactor.redactString(pending)
}

func (s *requestStreamRedactor) writeNormalized(text string) string {
	if text == "" {
		return ""
	}
	s.pending += text
	var output strings.Builder
	for len(s.pending) > 0 {
		index, secret := s.firstSecret()
		if index >= 0 {
			output.WriteString(s.pending[:index])
			output.WriteString(s.redactor.marker)
			s.pending = s.pending[index+len(secret):]
			continue
		}
		held := s.partialSecretSuffixBytes()
		output.WriteString(s.pending[:len(s.pending)-held])
		s.pending = s.pending[len(s.pending)-held:]
		break
	}
	return output.String()
}

func (s *requestStreamRedactor) normalizeUTF8(text string, final bool) string {
	data := make([]byte, 0, len(s.invalidBoundary)+len(text))
	data = append(data, s.invalidBoundary...)
	data = append(data, text...)
	s.invalidBoundary = nil

	var normalized strings.Builder
	for len(data) > 0 {
		if !final && !utf8.FullRune(data) {
			s.invalidBoundary = append(s.invalidBoundary, data...)
			break
		}
		r, size := utf8.DecodeRune(data)
		if r == utf8.RuneError && size == 1 {
			normalized.WriteRune(utf8.RuneError)
			data = data[1:]
			continue
		}
		normalized.Write(data[:size])
		data = data[size:]
	}
	return normalized.String()
}

func (s *requestStreamRedactor) firstSecret() (int, string) {
	first := -1
	matched := ""
	for _, secret := range s.redactor.values {
		index := strings.Index(s.pending, secret)
		if index < 0 || first >= 0 && index > first {
			continue
		}
		if first < 0 || index < first || len(secret) > len(matched) {
			first = index
			matched = secret
		}
	}
	return first, matched
}

func (s *requestStreamRedactor) partialSecretSuffixBytes() int {
	held := 0
	for _, secret := range s.redactor.values {
		maximum := min(len(s.pending), len(secret)-1)
		for size := maximum; size > held; size-- {
			if strings.HasSuffix(s.pending, secret[:size]) {
				held = size
				break
			}
		}
	}
	return held
}
