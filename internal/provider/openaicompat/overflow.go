package openaicompat

import (
	"bytes"
	"encoding/json"
	"io"
	"net/http"
	"regexp"
	"strconv"
	"strings"

	"github.com/baiyuqing/otto/internal/provider"
)

var (
	overflowMessagePatterns = []*regexp.Regexp{
		regexp.MustCompile(`\bmaximum context length\b`),
		regexp.MustCompile(`\bcontext window\b.{0,64}\b(exceeded|exceeds)\b`),
		regexp.MustCompile(`\binput tokens?\b.{0,64}\bexceed(ed|s)?\b.{0,64}\bcontext\b`),
	}
	requestedThenMaximumPattern    = regexp.MustCompile(`\brequested[ ]+([0-9]+)[ ]+tokens\b.{0,64}\bmaximum[ ]+([0-9]+)\b`)
	tokensThenContextMaxPattern    = regexp.MustCompile(`\b([0-9]+)[ ]+tokens\b.{0,64}\bmaximum context length is[ ]+([0-9]+)\b`)
	contextMaxThenRequestedPattern = regexp.MustCompile(`\bmaximum context length is[ ]+([0-9]+)[ ]+tokens\b.{0,64}\brequested[ ]+([0-9]+)[ ]+tokens\b`)
)

var overflowEvidence = map[string]string{
	"context_length_exceeded": "context_length_exceeded",
	"context_window_exceeded": "context_window_exceeded",
	"max_context_length":      "max_context_length",
}

func classifyContextOverflow(status int, body []byte) *provider.ContextOverflowError {
	if status != http.StatusBadRequest && status != http.StatusRequestEntityTooLarge && status != http.StatusUnprocessableEntity {
		return nil
	}
	if len(body) > maxErrorBody || !hasUniqueJSONKeys(body) {
		return nil
	}

	var root map[string]json.RawMessage
	if err := json.Unmarshal(body, &root); err != nil || root == nil {
		return nil
	}
	objects := make([]map[string]json.RawMessage, 0, 2)
	if raw, ok := root["error"]; ok {
		var nested map[string]json.RawMessage
		if json.Unmarshal(raw, &nested) == nil && nested != nil {
			objects = append(objects, nested)
		}
	}
	objects = append(objects, root)

	code := ""
	messages := make([]string, 0, len(objects))
	for _, object := range objects {
		if code == "" {
			code = recognizedOverflowValue(object["code"])
		}
		if code == "" {
			code = recognizedOverflowValue(object["type"])
		}
		if message, ok := jsonString(object["message"]); ok {
			messages = append(messages, message)
		}
	}

	messageMatch := false
	for _, message := range messages {
		if isOverflowMessage(message) {
			messageMatch = true
			break
		}
	}
	if code == "" && !messageMatch {
		return nil
	}

	overflow := &provider.ContextOverflowError{Status: status, Code: code}
	for _, message := range messages {
		current, maximum := extractTokenCounts(message)
		if current > 0 && maximum > 0 {
			overflow.CurrentTokens = current
			overflow.MaximumTokens = maximum
			break
		}
	}
	return overflow
}

func recognizedOverflowValue(raw json.RawMessage) string {
	value, ok := jsonString(raw)
	if !ok {
		return ""
	}
	return overflowEvidence[strings.ToLower(value)]
}

func jsonString(raw json.RawMessage) (string, bool) {
	if len(raw) == 0 {
		return "", false
	}
	var value string
	if err := json.Unmarshal(raw, &value); err != nil {
		return "", false
	}
	return value, true
}

func isOverflowMessage(message string) bool {
	normalized := normalizeOverflowMessage(message)
	if strings.Contains(normalized, "max_tokens") {
		return false
	}
	for _, pattern := range overflowMessagePatterns {
		if pattern.MatchString(normalized) {
			return true
		}
	}
	return false
}

func extractTokenCounts(message string) (int, int) {
	normalized := normalizeOverflowMessage(message)
	if matches := requestedThenMaximumPattern.FindStringSubmatch(normalized); matches != nil {
		return tokenPair(matches[1], matches[2])
	}
	if matches := tokensThenContextMaxPattern.FindStringSubmatch(normalized); matches != nil {
		return tokenPair(matches[1], matches[2])
	}
	if matches := contextMaxThenRequestedPattern.FindStringSubmatch(normalized); matches != nil {
		maximum, current := tokenPair(matches[1], matches[2])
		return current, maximum
	}
	return 0, 0
}

func tokenPair(currentText, maximumText string) (int, int) {
	current, err := strconv.Atoi(currentText)
	if err != nil || current <= 0 {
		return 0, 0
	}
	maximum, err := strconv.Atoi(maximumText)
	if err != nil || maximum <= 0 {
		return 0, 0
	}
	return current, maximum
}

func normalizeOverflowMessage(message string) string {
	message = strings.ReplaceAll(message, "\r", " ")
	message = strings.ReplaceAll(message, "\n", " ")
	return strings.ToLower(message)
}

func hasUniqueJSONKeys(body []byte) bool {
	decoder := json.NewDecoder(bytes.NewReader(body))
	decoder.UseNumber()
	if err := consumeUniqueJSONValue(decoder); err != nil {
		return false
	}
	_, err := decoder.Token()
	return err == io.EOF
}

func consumeUniqueJSONValue(decoder *json.Decoder) error {
	token, err := decoder.Token()
	if err != nil {
		return err
	}
	delimiter, ok := token.(json.Delim)
	if !ok {
		return nil
	}

	switch delimiter {
	case '{':
		seen := make(map[string]struct{})
		for decoder.More() {
			keyToken, err := decoder.Token()
			if err != nil {
				return err
			}
			key, ok := keyToken.(string)
			if !ok {
				return &json.SyntaxError{}
			}
			if _, duplicate := seen[key]; duplicate {
				return &duplicateJSONKeyError{}
			}
			seen[key] = struct{}{}
			if err := consumeUniqueJSONValue(decoder); err != nil {
				return err
			}
		}
		_, err = decoder.Token()
		return err
	case '[':
		for decoder.More() {
			if err := consumeUniqueJSONValue(decoder); err != nil {
				return err
			}
		}
		_, err = decoder.Token()
		return err
	default:
		return &json.SyntaxError{}
	}
}

type duplicateJSONKeyError struct{}

func (*duplicateJSONKeyError) Error() string { return "duplicate JSON key" }
