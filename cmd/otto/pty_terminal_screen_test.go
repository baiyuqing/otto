//go:build darwin

package main

import (
	"fmt"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/charmbracelet/x/ansi"
)

// ptyTerminalScreen is a test-only interpreter for the ANSI operations Bubble
// Tea emits in the resize PTY tests. Unknown operations fail closed.
type ptyTerminalScreen struct {
	width, height int
	x, y          int
	cells         [][]rune
	pending       []byte
	homePending   bool
	fullRedraws   int
}

func newPTYTerminalScreen(width, height int) *ptyTerminalScreen {
	screen := &ptyTerminalScreen{width: width, height: height}
	screen.cells = make([][]rune, height)
	for row := range screen.cells {
		screen.cells[row] = blankPTYRow(width)
	}
	return screen
}

func blankPTYRow(width int) []rune {
	row := make([]rune, width)
	for column := range row {
		row[column] = ' '
	}
	return row
}

func (s *ptyTerminalScreen) Write(p []byte) (int, error) {
	s.pending = append(s.pending, p...)
	for len(s.pending) > 0 {
		consumed, complete, err := s.consume()
		if err != nil {
			return 0, err
		}
		if !complete {
			break
		}
		s.pending = s.pending[consumed:]
	}
	return len(p), nil
}

func (s *ptyTerminalScreen) consume() (int, bool, error) {
	if s.pending[0] == '\x1b' {
		return s.consumeCSI()
	}

	s.homePending = false
	switch s.pending[0] {
	case '\r':
		s.x = 0
		return 1, true, nil
	case '\n':
		s.lineFeed()
		return 1, true, nil
	case '\b':
		s.x = max(s.x-1, 0)
		return 1, true, nil
	case '\t':
		if s.width > 0 {
			s.x = min(((s.x/8)+1)*8, s.width-1)
		}
		return 1, true, nil
	}
	if s.pending[0] < utf8.RuneSelf {
		if s.pending[0] >= ' ' && s.pending[0] != 0x7f {
			s.putRune(rune(s.pending[0]))
		}
		return 1, true, nil
	}
	if !utf8.FullRune(s.pending) {
		return 0, false, nil
	}
	r, size := utf8.DecodeRune(s.pending)
	if r == utf8.RuneError && size == 1 {
		return 0, false, fmt.Errorf("invalid UTF-8 in terminal output")
	}
	s.putRune(r)
	return size, true, nil
}

func (s *ptyTerminalScreen) consumeCSI() (int, bool, error) {
	if len(s.pending) < 2 {
		return 0, false, nil
	}
	if s.pending[1] != '[' {
		return 0, false, fmt.Errorf("unsupported terminal escape %q", s.pending[:2])
	}
	for index := 2; index < len(s.pending); index++ {
		if s.pending[index] < 0x40 || s.pending[index] > 0x7e {
			continue
		}
		sequence := string(s.pending[:index+1])
		if err := s.applyCSI(string(s.pending[2:index]), s.pending[index]); err != nil {
			return 0, false, fmt.Errorf("%w in %q", err, sequence)
		}
		return index + 1, true, nil
	}
	return 0, false, nil
}

func (s *ptyTerminalScreen) applyCSI(rawParams string, final byte) error {
	wasHome := s.homePending
	s.homePending = false

	switch final {
	case 'm', 'h', 'l': // Styling and terminal modes do not change cells.
		return nil
	case 'H', 'f':
		row := ptyCSIParam(rawParams, 0, 1) - 1
		column := ptyCSIParam(rawParams, 1, 1) - 1
		s.moveTo(column, row)
		s.homePending = s.x == 0 && s.y == 0
	case 'd':
		s.moveTo(s.x, ptyCSIParam(rawParams, 0, 1)-1)
	case 'J':
		if mode := ptyCSIParam(rawParams, 0, 0); mode == 2 {
			s.clear()
			if wasHome {
				s.fullRedraws++
			}
		} else {
			return fmt.Errorf("unsupported erase-display mode %d", mode)
		}
	case 'K':
		mode := ptyCSIParam(rawParams, 0, 0)
		if mode != 0 && mode != 2 {
			return fmt.Errorf("unsupported erase-line mode %d", mode)
		}
		from := s.x
		if mode == 2 {
			from = 0
		}
		s.eraseRow(s.y, from, s.width-1)
	case 'X':
		count := max(ptyCSIParam(rawParams, 0, 1), 1)
		s.eraseRow(s.y, s.x, min(s.x+count-1, s.width-1))
	case 'L':
		s.insertLines(max(ptyCSIParam(rawParams, 0, 1), 1))
	default:
		return fmt.Errorf("unsupported terminal CSI")
	}
	return nil
}

func ptyCSIParam(raw string, index, fallback int) int {
	raw = strings.TrimLeft(raw, "?><!")
	parts := strings.Split(raw, ";")
	if index >= len(parts) || parts[index] == "" {
		return fallback
	}
	value, err := strconv.Atoi(parts[index])
	if err != nil {
		return fallback
	}
	if value == 0 && fallback == 1 {
		return fallback
	}
	return value
}

func (s *ptyTerminalScreen) moveTo(x, y int) {
	if s.width == 0 || s.height == 0 {
		s.x, s.y = 0, 0
		return
	}
	s.x = min(max(x, 0), s.width-1)
	s.y = min(max(y, 0), s.height-1)
}

func (s *ptyTerminalScreen) putRune(r rune) {
	if s.width == 0 || s.height == 0 {
		return
	}
	width := ansi.StringWidth(string(r))
	if width <= 0 {
		return
	}
	if s.x >= s.width {
		s.x = 0
		s.lineFeed()
	}
	s.cells[s.y][s.x] = r
	for offset := 1; offset < width && s.x+offset < s.width; offset++ {
		s.cells[s.y][s.x+offset] = ' '
	}
	s.x += width
}

func (s *ptyTerminalScreen) lineFeed() {
	if s.height == 0 {
		return
	}
	if s.y < s.height-1 {
		s.y++
		return
	}
	copy(s.cells, s.cells[1:])
	s.cells[s.height-1] = blankPTYRow(s.width)
}

func (s *ptyTerminalScreen) clear() {
	for row := range s.cells {
		s.cells[row] = blankPTYRow(s.width)
	}
}

func (s *ptyTerminalScreen) insertLines(count int) {
	if s.height == 0 {
		return
	}
	count = min(count, s.height-s.y)
	for row := s.height - 1; row >= s.y+count; row-- {
		copy(s.cells[row], s.cells[row-count])
	}
	for row := s.y; row < s.y+count; row++ {
		s.cells[row] = blankPTYRow(s.width)
	}
}

func (s *ptyTerminalScreen) eraseRow(row, from, to int) {
	if row < 0 || row >= s.height || s.width == 0 {
		return
	}
	from, to = max(from, 0), min(to, s.width-1)
	for column := from; column <= to; column++ {
		s.cells[row][column] = ' '
	}
}

func (s *ptyTerminalScreen) FullRedraws() int {
	return s.fullRedraws
}

func (s *ptyTerminalScreen) Complete() bool {
	return len(s.pending) == 0
}

func (s *ptyTerminalScreen) String() string {
	lines := make([]string, len(s.cells))
	for row := range s.cells {
		lines[row] = string(s.cells[row])
	}
	return strings.Join(lines, "\n")
}

func ptyScreenHasResumeEvidence(screen *ptyTerminalScreen) bool {
	if screen == nil || screen.FullRedraws() == 0 || !screen.Complete() {
		return false
	}
	content := screen.String()
	return strings.Contains(content, selectedAssistantTranscript) &&
		strings.Contains(content, selectedResumeSessionID) &&
		!strings.Contains(content, "Resume Session")
}
