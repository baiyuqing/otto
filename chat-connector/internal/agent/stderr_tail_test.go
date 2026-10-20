package agent

import (
	"strings"
	"testing"
)

func TestTailBoundsALineWithoutNewline(t *testing.T) {
	t.Parallel()
	tl := &tail{}
	for i := 0; i < 3; i++ {
		if _, err := tl.Write([]byte(strings.Repeat("x", stderrLineBytes/2+1))); err != nil {
			t.Fatal(err)
		}
		if len(tl.buf) > stderrLineBytes {
			t.Fatalf("buf = %d bytes, want <= %d", len(tl.buf), stderrLineBytes)
		}
	}
	if len(tl.last) == 0 {
		t.Fatal("no line recorded for output past the cap")
	}
}
