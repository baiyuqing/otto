package agent

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"sort"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/session"
)

const redactionMarker = "█"

// Redactor removes resolved secret values at the provider-neutral agent
// boundary. Its source values remain encapsulated and are never exposed
// through agent options or errors.
type Redactor struct {
	values []string
	marker string
}

func NewRedactor(values []string) *Redactor {
	seen := make(map[string]struct{}, len(values))
	redactor := &Redactor{}
	for _, value := range values {
		if value == "" || !utf8.ValidString(value) {
			continue
		}
		if _, exists := seen[value]; exists {
			continue
		}
		seen[value] = struct{}{}
		redactor.values = append(redactor.values, strings.Clone(value))
	}
	sort.Slice(redactor.values, func(i, j int) bool {
		if len(redactor.values[i]) != len(redactor.values[j]) {
			return len(redactor.values[i]) > len(redactor.values[j])
		}
		return redactor.values[i] < redactor.values[j]
	})
	redactor.marker = safeRedactionMarker(redactor.values)
	return redactor
}

func (r *Redactor) RedactString(text string) string {
	if r == nil || len(r.values) == 0 || text == "" {
		return text
	}
	for _, value := range r.values {
		text = strings.ReplaceAll(text, value, r.marker)
	}
	return text
}

func (r *Redactor) RedactJSONStrings(raw json.RawMessage) json.RawMessage {
	if r == nil || len(r.values) == 0 {
		return append(json.RawMessage(nil), raw...)
	}
	value, err := decodePairPreservingJSON(raw)
	if err != nil {
		return json.RawMessage("null")
	}
	value = redactJSONValueStrings(r, value)
	encoded, err := json.Marshal(value)
	if err != nil {
		return json.RawMessage("null")
	}
	return json.RawMessage(encoded)
}

func (r *Redactor) RedactError(err error) error {
	if err == nil || r == nil || len(r.values) == 0 {
		return err
	}
	message := r.RedactString(err.Error())
	if message == err.Error() {
		return err
	}
	return &redactedBoundaryError{
		message:          message,
		canceled:         errors.Is(err, context.Canceled),
		deadlineExceeded: errors.Is(err, context.DeadlineExceeded),
		fatalPersistence: errors.Is(err, session.ErrFatalPersistence),
		emptyUserText:    errors.Is(err, ErrEmptyUserText),
	}
}

type redactedBoundaryError struct {
	message          string
	canceled         bool
	deadlineExceeded bool
	fatalPersistence bool
	emptyUserText    bool
}

func (e *redactedBoundaryError) Error() string { return e.message }
func (e *redactedBoundaryError) Is(target error) bool {
	return target == context.Canceled && e.canceled ||
		target == context.DeadlineExceeded && e.deadlineExceeded ||
		target == session.ErrFatalPersistence && e.fatalPersistence ||
		target == ErrEmptyUserText && e.emptyUserText
}

type jsonObject []jsonMember

type jsonMember struct {
	key   string
	value any
}

const maximumJSONDepth = 10_000

func decodePairPreservingJSON(raw json.RawMessage) (any, error) {
	decoder := json.NewDecoder(bytes.NewReader(raw))
	decoder.UseNumber()
	value, err := decodeJSONValue(decoder, 0)
	if err != nil {
		return nil, err
	}
	if err := ensureJSONEOF(decoder); err != nil {
		return nil, err
	}
	return value, nil
}

func decodeJSONValue(decoder *json.Decoder, depth int) (any, error) {
	token, err := decoder.Token()
	if err != nil {
		return nil, err
	}
	delimiter, isDelimiter := token.(json.Delim)
	if !isDelimiter {
		return token, nil
	}
	if depth >= maximumJSONDepth {
		return nil, errors.New("maximum JSON depth exceeded")
	}

	switch delimiter {
	case '{':
		object := make(jsonObject, 0)
		for decoder.More() {
			keyToken, err := decoder.Token()
			if err != nil {
				return nil, err
			}
			key, ok := keyToken.(string)
			if !ok {
				return nil, errors.New("JSON object member name is not a string")
			}
			value, err := decodeJSONValue(decoder, depth+1)
			if err != nil {
				return nil, err
			}
			object = append(object, jsonMember{key: key, value: value})
		}
		if err := consumeJSONDelimiter(decoder, '}'); err != nil {
			return nil, err
		}
		return object, nil
	case '[':
		array := make([]any, 0)
		for decoder.More() {
			value, err := decodeJSONValue(decoder, depth+1)
			if err != nil {
				return nil, err
			}
			array = append(array, value)
		}
		if err := consumeJSONDelimiter(decoder, ']'); err != nil {
			return nil, err
		}
		return array, nil
	default:
		return nil, errors.New("unexpected closing JSON delimiter")
	}
}

func consumeJSONDelimiter(decoder *json.Decoder, want json.Delim) error {
	token, err := decoder.Token()
	if err != nil {
		return err
	}
	if delimiter, ok := token.(json.Delim); !ok || delimiter != want {
		return errors.New("unexpected JSON delimiter")
	}
	return nil
}

func redactJSONValueStrings(redactor *Redactor, value any) any {
	switch value := value.(type) {
	case string:
		return redactor.RedactString(value)
	case jsonObject:
		redactedMembers := make(jsonObject, len(value))
		for index, member := range value {
			redactedMembers[index] = jsonMember{
				key:   redactor.RedactString(member.key),
				value: redactJSONValueStrings(redactor, member.value),
			}
		}

		redacted := make(map[string]any, len(redactedMembers))
		for _, member := range redactedMembers {
			if _, collision := redacted[member.key]; collision {
				// Raw duplicates, normalized aliases, and redaction collisions all
				// lose their values rather than selecting an attacker-controlled winner.
				redacted[member.key] = nil
				continue
			}
			redacted[member.key] = member.value
		}
		return redacted
	case []any:
		for index, item := range value {
			value[index] = redactJSONValueStrings(redactor, item)
		}
	}
	return value
}

func safeRedactionMarker(values []string) string {
	if markerRuneAbsent(redactionMarker, values) {
		return redactionMarker
	}
	for candidate := rune(0xE000); candidate <= 0xF8FF; candidate++ {
		marker := string(candidate)
		if markerRuneAbsent(marker, values) {
			return marker
		}
	}
	for candidate := rune(1); candidate <= utf8.MaxRune; candidate++ {
		if utf8.ValidRune(candidate) && !unicode.IsControl(candidate) {
			marker := string(candidate)
			if markerRuneAbsent(marker, values) {
				return marker
			}
		}
	}
	return ""
}

func markerRuneAbsent(marker string, values []string) bool {
	for _, value := range values {
		if strings.Contains(value, marker) {
			return false
		}
	}
	return true
}

func ensureJSONEOF(decoder *json.Decoder) error {
	var extra any
	if err := decoder.Decode(&extra); err != io.EOF {
		if err == nil {
			return errors.New("multiple JSON values")
		}
		return err
	}
	return nil
}

type streamRedactor struct {
	redactor *Redactor
	pending  string
}

func (r *Redactor) newStream() *streamRedactor {
	return &streamRedactor{redactor: r}
}

func (s *streamRedactor) Write(text string) string {
	if text == "" {
		return ""
	}
	if s == nil || s.redactor == nil || len(s.redactor.values) == 0 {
		return text
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

func (s *streamRedactor) Flush() string {
	if s == nil || s.pending == "" {
		return ""
	}
	pending := s.pending
	s.pending = ""
	return s.redactor.RedactString(pending)
}

func (s *streamRedactor) firstSecret() (int, string) {
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

func (s *streamRedactor) partialSecretSuffixBytes() int {
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
