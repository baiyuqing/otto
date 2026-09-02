package safetext

import (
	"encoding/json"
	"slices"
	"strings"
	"unicode"
	"unicode/utf8"
)

const (
	MaxSecretValues      = 512
	MaxSecretBytes       = 1 << 20
	MaxDynamicValues     = 64
	MaxDynamicBytes      = 16 << 10
	MaxDynamicValueBytes = 8 << 10
)

const preferredDynamicMarker = '\uE000'

const dynamicMarkerCount = 64

// DynamicRedactionMarker returns the shared exact-redaction marker for bounded,
// fully-known secret sets. If the set exceeds the exact redaction capability or
// no safe literal JSON marker remains, exact dynamic redaction must be disabled.
func DynamicRedactionMarker(values []string) (string, bool) {
	if !supportsDynamicRedaction(values) {
		return "", false
	}
	marker := SharedRedactionMarker(values)
	return marker, marker != ""
}

func SharedRedactionMarker(values []string) string {
	usedRunes := make(map[rune]struct{}, len(values))
	for _, value := range values {
		for _, candidate := range value {
			usedRunes[candidate] = struct{}{}
		}
	}
	for candidate := preferredDynamicMarker; candidate < preferredDynamicMarker+dynamicMarkerCount; candidate++ {
		if !utf8.ValidRune(candidate) || unicode.IsControl(candidate) {
			continue
		}
		if _, used := usedRunes[candidate]; used {
			continue
		}
		marker := string(candidate)
		encoded, err := json.Marshal(marker)
		if err != nil || len(encoded) < 2 {
			continue
		}
		serialized := string(encoded[1 : len(encoded)-1])
		if containsRetainedForm(marker, values) || containsRetainedForm(serialized, values) {
			continue
		}
		return marker
	}
	return ""
}

func supportsDynamicRedaction(values []string) bool {
	if len(values) > MaxDynamicValues {
		return false
	}
	total := 0
	for _, value := range values {
		if len(value) > MaxDynamicValueBytes {
			return false
		}
		total += len(value)
		if total > MaxDynamicBytes {
			return false
		}
	}
	return true
}

func containsRetainedForm(candidate string, values []string) bool {
	for _, value := range values {
		if value != "" && strings.Contains(candidate, value) {
			return true
		}
	}
	return false
}

type SecretCollector struct {
	seen  map[string]struct{}
	bytes int
}

func NewSecretCollector() *SecretCollector {
	return &SecretCollector{seen: make(map[string]struct{})}
}

func (c *SecretCollector) Add(value string) bool {
	if c == nil {
		return false
	}
	for _, form := range SecretForms(value) {
		if !c.AddForm(form) {
			return false
		}
	}
	return true
}

func (c *SecretCollector) AddForm(value string) bool {
	if c == nil {
		return false
	}
	value = CanonicalizeUTF8(value)
	if value == "" {
		return true
	}
	if _, duplicate := c.seen[value]; duplicate {
		return true
	}
	if len(c.seen) >= MaxSecretValues || c.bytes > MaxSecretBytes-len(value) {
		return false
	}
	c.seen[strings.Clone(value)] = struct{}{}
	c.bytes += len(value)
	return true
}

func (c *SecretCollector) Values() []string {
	if c == nil {
		return nil
	}
	values := make([]string, 0, len(c.seen))
	for value := range c.seen {
		values = append(values, value)
	}
	SortLongestFirst(values)
	return values
}

func (c *SecretCollector) Len() int {
	if c == nil {
		return 0
	}
	return len(c.seen)
}

func (c *SecretCollector) Bytes() int {
	if c == nil {
		return 0
	}
	return c.bytes
}

func SortLongestFirst(values []string) {
	if len(values) < 2 {
		return
	}
	slices.SortFunc(values, func(left, right string) int {
		if len(left) != len(right) {
			return len(right) - len(left)
		}
		return strings.Compare(left, right)
	})
}
