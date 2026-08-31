package tool

import (
	"io"
	"strings"
)

type Result struct {
	Content string
	IsError bool
}

type cappedByteCollector struct {
	limit     int
	buf       []byte
	discarded int
}

func newCappedByteCollector(limit int) *cappedByteCollector {
	if limit < 0 {
		limit = 0
	}
	return &cappedByteCollector{limit: limit}
}

func (c *cappedByteCollector) Write(p []byte) (int, error) {
	if len(c.buf) < c.limit {
		remaining := c.limit - len(c.buf)
		if remaining > len(p) {
			remaining = len(p)
		}
		c.buf = append(c.buf, p[:remaining]...)
		c.discarded += len(p) - remaining
		return len(p), nil
	}
	c.discarded += len(p)
	return len(p), nil
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
	var redacted strings.Builder
	writer := newExactRedactingWriterWithMarker(&redacted, redactionValues, marker)
	if _, err := io.WriteString(writer, value); err != nil {
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

		w.write(w.marker)
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

var _ io.Writer = (*cappedByteCollector)(nil)
var _ io.Writer = (*exactRedactingWriter)(nil)
