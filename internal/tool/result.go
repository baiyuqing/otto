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

type exactRedactingWriter struct {
	destination io.Writer
	values      []string
	pending     string
	maxValueLen int
	err         error
}

func newExactRedactingWriter(destination io.Writer, values []string) *exactRedactingWriter {
	writer := &exactRedactingWriter{destination: destination}
	seen := make(map[string]struct{})
	for _, value := range values {
		if value == "" {
			continue
		}
		if _, exists := seen[value]; exists {
			continue
		}
		seen[value] = struct{}{}
		writer.values = append(writer.values, value)
		if len(value) > writer.maxValueLen {
			writer.maxValueLen = len(value)
		}
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

func redactExactText(value string, redactionValues []string) (string, error) {
	var redacted strings.Builder
	writer := newExactRedactingWriter(&redacted, redactionValues)
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
		matchIndex := -1
		matchValue := ""
		for _, value := range w.values {
			index := strings.Index(w.pending, value)
			if index >= 0 && (matchIndex < 0 || index < matchIndex || index == matchIndex && len(value) > len(matchValue)) {
				matchIndex = index
				matchValue = value
			}
		}
		if matchIndex >= 0 {
			w.write(w.pending[:matchIndex])
			w.write("[REDACTED]")
			w.pending = w.pending[matchIndex+len(matchValue):]
			continue
		}

		keep := w.maxValueLen - 1
		if final {
			keep = 0
		}
		if len(w.pending) <= keep {
			return
		}
		writeLength := len(w.pending) - keep
		w.write(w.pending[:writeLength])
		w.pending = w.pending[writeLength:]
	}
}

func (w *exactRedactingWriter) write(value string) {
	if value == "" || w.err != nil {
		return
	}
	_, w.err = io.WriteString(w.destination, value)
}

var _ io.Writer = (*cappedByteCollector)(nil)
var _ io.Writer = (*exactRedactingWriter)(nil)
