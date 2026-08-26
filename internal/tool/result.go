package tool

import "io"

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

var _ io.Writer = (*cappedByteCollector)(nil)
