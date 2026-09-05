package tool

import (
	"io"
	"strings"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/safetext"
)

type Result struct {
	Content string
	// PersistedContent replaces Content in the durable session transcript when
	// non-nil. A pointed-to empty string is an intentional empty override;
	// Content still reaches the live EventToolCallFinished event unchanged.
	PersistedContent *string
	IsError          bool
}

type cappedByteCollector struct {
	limit     int
	buf       []byte
	discarded int
	sealed    bool
}

func newCappedByteCollector(limit int) *cappedByteCollector {
	if limit < 0 {
		limit = 0
	}
	return &cappedByteCollector{limit: limit}
}

func (c *cappedByteCollector) Write(p []byte) (int, error) {
	original := len(p)
	if c.sealed {
		c.discarded += original
		return original, nil
	}
	remaining := c.limit - len(c.buf)
	if remaining <= 0 {
		c.discarded += original
		c.sealed = true
		return original, nil
	}
	if remaining > len(p) {
		remaining = len(p)
	}
	retained := completeUTF8Prefix(p, remaining)
	c.buf = append(c.buf, p[:retained]...)
	c.discarded += original - retained
	if retained < original {
		c.sealed = true
	}
	return original, nil
}

func (c *cappedByteCollector) WriteAtomic(p []byte) (int, error) {
	if c.sealed {
		c.discarded += len(p)
		return len(p), nil
	}
	if len(p) <= c.limit-len(c.buf) && utf8.Valid(p) {
		c.buf = append(c.buf, p...)
	} else {
		c.discarded += len(p)
		c.sealed = true
	}
	return len(p), nil
}

func completeUTF8Prefix(value []byte, limit int) int {
	if limit > len(value) {
		limit = len(value)
	}
	index := 0
	for index < limit {
		_, size := utf8.DecodeRune(value[index:limit])
		if size == 1 && value[index] >= utf8.RuneSelf {
			break
		}
		index += size
	}
	return index
}

func (c *cappedByteCollector) Bytes() []byte {
	return append([]byte(nil), c.buf...)
}

func (c *cappedByteCollector) Discarded() int {
	return c.discarded
}

const legacyExactRedactionMarker = "[REDACTED]"

type exactRedactingWriter struct {
	destination io.Writer
	values      []string
	marker      string
	pending     string
	err         error
}

func newExactRedactingWriter(destination io.Writer, values []string) *exactRedactingWriter {
	return newExactRedactingWriterWithMarker(destination, values, legacyExactRedactionMarker)
}

func newExactRedactingWriterWithMarker(destination io.Writer, values []string, marker string) *exactRedactingWriter {
	writer := &exactRedactingWriter{destination: destination, marker: strings.Clone(marker)}
	seen := make(map[string]struct{})
	for _, value := range values {
		value = safetext.CanonicalizeUTF8(value)
		if value == "" {
			continue
		}
		value = strings.Clone(value)
		if _, exists := seen[value]; exists {
			continue
		}
		seen[value] = struct{}{}
		writer.values = append(writer.values, value)
	}
	return writer
}

func (w *exactRedactingWriter) Write(data []byte) (int, error) {
	if w.err != nil {
		return 0, w.err
	}
	if len(w.values) == 0 {
		return w.destination.Write(data)
	}
	w.pending += string(data)
	w.process(false)
	if w.err != nil {
		return 0, w.err
	}
	return len(data), nil
}

func (w *exactRedactingWriter) Flush() error {
	w.process(true)
	return w.err
}

func redactExactText(value string, redactionValues []string, marker string) (string, error) {
	if marker == "" && len(redactionValues) > 0 {
		return "", nil
	}
	var redacted strings.Builder
	writer := newExactRedactingWriterWithMarker(&redacted, redactionValues, marker)
	normalizer := newUTF8NormalizingWriter(writer)
	if _, err := io.WriteString(normalizer, value); err != nil {
		return "", err
	}
	if err := normalizer.Flush(); err != nil {
		return "", err
	}
	if err := writer.Flush(); err != nil {
		return "", err
	}
	return redacted.String(), nil
}

func (w *exactRedactingWriter) process(final bool) {
	for w.err == nil && w.pending != "" {
		matchIndex, matchValue, unresolved := w.leftmostCandidate(final)
		if matchIndex < 0 {
			w.write(w.pending)
			w.pending = ""
			return
		}
		if matchIndex > 0 {
			w.write(w.pending[:matchIndex])
			w.pending = w.pending[matchIndex:]
			continue
		}
		if unresolved {
			return
		}

		w.writeMarker()
		w.pending = w.pending[len(matchValue):]
	}
}

func (w *exactRedactingWriter) leftmostCandidate(final bool) (int, string, bool) {
	for index := 0; index < len(w.pending); index++ {
		remaining := w.pending[index:]
		longestMatch := ""
		unresolved := false
		for _, value := range w.values {
			switch {
			case strings.HasPrefix(remaining, value):
				if len(value) > len(longestMatch) {
					longestMatch = value
				}
			case !final && len(remaining) < len(value) && strings.HasPrefix(value, remaining):
				unresolved = true
			}
		}
		if longestMatch != "" || unresolved {
			return index, longestMatch, unresolved
		}
	}
	return -1, "", false
}

func (w *exactRedactingWriter) write(value string) {
	if value == "" || w.err != nil {
		return
	}
	_, w.err = io.WriteString(w.destination, value)
}

func (w *exactRedactingWriter) writeMarker() {
	if w.marker == "" || w.err != nil {
		return
	}
	if destination, ok := w.destination.(interface {
		WriteAtomic([]byte) (int, error)
	}); ok {
		_, w.err = destination.WriteAtomic([]byte(w.marker))
		return
	}
	w.write(w.marker)
}

type utf8NormalizingWriter struct {
	destination io.Writer
	pending     []byte
	err         error
}

func newUTF8NormalizingWriter(destination io.Writer) *utf8NormalizingWriter {
	return &utf8NormalizingWriter{destination: destination}
}

func (w *utf8NormalizingWriter) Write(data []byte) (int, error) {
	if w.err != nil {
		return 0, w.err
	}
	original := len(data)
	w.pending = append(w.pending, data...)
	w.process(false)
	if w.err != nil {
		return 0, w.err
	}
	return original, nil
}

func (w *utf8NormalizingWriter) Flush() error {
	w.process(true)
	return w.err
}

func (w *utf8NormalizingWriter) process(final bool) {
	for w.err == nil && len(w.pending) > 0 {
		if !final && !utf8.FullRune(w.pending) {
			return
		}
		r, size := utf8.DecodeRune(w.pending)
		if r == utf8.RuneError && size == 1 {
			w.writeString(string(utf8.RuneError))
			w.pending = w.pending[1:]
			continue
		}
		w.write(w.pending[:size])
		w.pending = w.pending[size:]
	}
}

func (w *utf8NormalizingWriter) write(value []byte) {
	if len(value) == 0 || w.err != nil {
		return
	}
	_, w.err = w.destination.Write(value)
}

func (w *utf8NormalizingWriter) writeString(value string) {
	if value == "" || w.err != nil {
		return
	}
	_, w.err = io.WriteString(w.destination, value)
}

var _ io.Writer = (*cappedByteCollector)(nil)
var _ io.Writer = (*exactRedactingWriter)(nil)
var _ io.Writer = (*utf8NormalizingWriter)(nil)
