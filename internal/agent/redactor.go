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

	"github.com/baiyuqing/otto/internal/safetext"
	"github.com/baiyuqing/otto/internal/session"
)

const redactionMarker = "█"

// Redactor removes resolved secret values at the provider-neutral agent
// boundary. Its source values remain encapsulated and are never exposed
// through agent options or errors.
type Redactor struct {
	values   []string
	marker   string
	complete bool
}

func NewRedactor(values []string) *Redactor {
	return NewRedactorWithCompleteness(values, true)
}

func NewRedactorWithCompleteness(values []string, complete bool) *Redactor {
	redactor := &Redactor{complete: complete}
	if !complete {
		return redactor
	}
	seen := make(map[string]struct{}, len(values))
	for _, value := range values {
		value = safetext.CanonicalizeUTF8(value)
		if value == "" {
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
	if text == "" {
		return text
	}
	text = safetext.CanonicalizeUTF8(text)
	if r != nil && !r.complete {
		return ""
	}
	if r == nil || len(r.values) == 0 {
		return text
	}
	if r.marker != "" {
		for _, value := range r.values {
			text = strings.ReplaceAll(text, value, r.marker)
		}
		return text
	}
	for {
		before := len(text)
		for _, value := range r.values {
			text = strings.ReplaceAll(text, value, "")
		}
		if len(text) == before {
			return text
		}
	}
}

func (r *Redactor) RedactJSONStrings(raw json.RawMessage) json.RawMessage {
	if r != nil && !r.complete {
		return json.RawMessage("null")
	}
	if r == nil || len(r.values) == 0 {
		if utf8.Valid(raw) {
			return append(json.RawMessage(nil), raw...)
		}
		canonical := json.RawMessage(safetext.CanonicalizeUTF8(string(raw)))
		if !json.Valid(canonical) {
			return json.RawMessage("null")
		}
		return append(json.RawMessage(nil), canonical...)
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
	if err == nil || r == nil {
		return err
	}
	message := r.RedactString(err.Error())
	if message == err.Error() {
		return err
	}
	return &redactedBoundaryError{
		message:                  message,
		canceled:                 errors.Is(err, context.Canceled),
		deadlineExceeded:         errors.Is(err, context.DeadlineExceeded),
		fatalPersistence:         errors.Is(err, session.ErrFatalPersistence),
		emptyUserText:            errors.Is(err, ErrEmptyUserText),
		invalidCompactionSummary: errors.Is(err, ErrInvalidCompactionSummary),
	}
}

type redactedBoundaryError struct {
	message                  string
	canceled                 bool
	deadlineExceeded         bool
	fatalPersistence         bool
	emptyUserText            bool
	invalidCompactionSummary bool
}

func (e *redactedBoundaryError) Error() string { return e.message }
func (e *redactedBoundaryError) Is(target error) bool {
	return target == context.Canceled && e.canceled ||
		target == context.DeadlineExceeded && e.deadlineExceeded ||
		target == session.ErrFatalPersistence && e.fatalPersistence ||
		target == ErrEmptyUserText && e.emptyUserText ||
		target == ErrInvalidCompactionSummary && e.invalidCompactionSummary
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
	usedRunes := make(map[rune]struct{})
	for _, value := range values {
		for _, candidate := range value {
			usedRunes[candidate] = struct{}{}
		}
	}
	isSafeRune := func(candidate rune) bool {
		if !utf8.ValidRune(candidate) || unicode.IsControl(candidate) {
			return false
		}
		_, used := usedRunes[candidate]
		return !used
	}
	preferred, _ := utf8.DecodeRuneInString(redactionMarker)
	if isSafeRune(preferred) {
		return redactionMarker
	}
	for candidate := rune(0xE000); candidate <= 0xF8FF; candidate++ {
		if isSafeRune(candidate) {
			return string(candidate)
		}
	}
	for candidate := rune(1); candidate <= utf8.MaxRune; candidate++ {
		if isSafeRune(candidate) {
			return string(candidate)
		}
	}

	return ""
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
	redactor        *Redactor
	pending         string
	invalidBoundary []byte
}

func (r *Redactor) newStream() *streamRedactor {
	return &streamRedactor{redactor: r}
}

func (s *streamRedactor) Write(text string) string {
	if s == nil || text == "" {
		return ""
	}
	if s.redactor != nil && !s.redactor.complete {
		return ""
	}
	return s.writeNormalized(s.normalizeUTF8(text, false))
}

func (s *streamRedactor) writeNormalized(text string) string {
	if text == "" {
		return ""
	}
	if s.redactor == nil || len(s.redactor.values) == 0 {
		return text
	}
	s.pending += text
	if s.redactor.marker == "" {
		return ""
	}
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
	if s == nil {
		return ""
	}
	if s.redactor != nil && !s.redactor.complete {
		s.pending = ""
		s.invalidBoundary = nil
		return ""
	}
	normalized := s.normalizeUTF8("", true)
	if s.redactor == nil || len(s.redactor.values) == 0 {
		return normalized
	}
	s.pending += normalized
	pending := s.pending
	s.pending = ""
	return s.redactor.RedactString(pending)
}

func (s *streamRedactor) normalizeUTF8(text string, final bool) string {
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
