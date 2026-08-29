//go:build darwin

package main

import (
	"fmt"
	"sort"
	"strconv"
	"strings"
	"testing"
	"unicode"
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
	cursorVisible bool
	fullRedraws   int
	acceptedCSI   map[string]struct{}
}

func newPTYTerminalScreen(width, height int) *ptyTerminalScreen {
	screen := &ptyTerminalScreen{width: width, height: height, acceptedCSI: make(map[string]struct{})}
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
		if len(s.pending) < 2 {
			return 0, false, nil
		}
		if s.pending[1] == 'M' {
			s.reverseIndex()
			return 2, true, nil
		}
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
		if s.pending[0] < ' ' || s.pending[0] == 0x7f {
			return 0, false, fmt.Errorf("unsupported terminal control 0x%02x", s.pending[0])
		}
		s.putRune(rune(s.pending[0]))
		return 1, true, nil
	}
	if !utf8.FullRune(s.pending) {
		return 0, false, nil
	}
	r, size := utf8.DecodeRune(s.pending)
	if r == utf8.RuneError && size == 1 {
		return 0, false, fmt.Errorf("invalid UTF-8 in terminal output")
	}
	if unicode.IsControl(r) {
		return 0, false, fmt.Errorf("unsupported Unicode terminal control U+%04X", r)
	}
	s.putRune(r)
	return size, true, nil
}

const (
	maxPTYCSISequence = 128
	maxPTYCSIParams   = 16
	maxPTYCSIParam    = 1_000_000
)

func (s *ptyTerminalScreen) consumeCSI() (int, bool, error) {
	if len(s.pending) < 2 {
		return 0, false, nil
	}
	if s.pending[1] != '[' {
		return 0, false, fmt.Errorf("unsupported terminal escape %q", s.pending[:2])
	}
	for index := 2; index < len(s.pending); index++ {
		current := s.pending[index]
		switch {
		case current >= 0x30 && current <= 0x3f:
			// Parameter bytes are validated for each supported final below.
		case current >= 0x40 && current <= 0x7e:
			sequence := string(s.pending[:index+1])
			if err := s.applyCSI(string(s.pending[2:index]), current); err != nil {
				return 0, false, fmt.Errorf("%w in %q", err, sequence)
			}
			return index + 1, true, nil
		case current >= 0x20 && current <= 0x2f:
			return 0, false, fmt.Errorf("unsupported CSI intermediate 0x%02x", current)
		default:
			return 0, false, fmt.Errorf("invalid CSI byte 0x%02x", current)
		}
		if index+1 >= maxPTYCSISequence {
			return 0, false, fmt.Errorf("CSI sequence exceeds %d bytes", maxPTYCSISequence)
		}
	}
	return 0, false, nil
}

func (s *ptyTerminalScreen) applyCSI(rawParams string, final byte) error {
	wasHome := s.homePending
	s.homePending = false

	switch final {
	case 'm':
		if err := validatePTYSGRParams(rawParams); err != nil {
			return err
		}
	case 'h', 'l':
		if rawParams != "?25" {
			return fmt.Errorf("unsupported terminal mode %q", rawParams)
		}
		s.cursorVisible = final == 'h'
	case 'H', 'f':
		params, err := parsePTYCSIParams(rawParams, 2, true)
		if err != nil {
			return err
		}
		row := ptyCSIParam(params, 0, 1) - 1
		column := ptyCSIParam(params, 1, 1) - 1
		s.moveTo(column, row)
		s.homePending = s.x == 0 && s.y == 0
	case 'd':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		s.moveTo(s.x, ptyCSIParam(params, 0, 1)-1)
	case 'G':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		s.moveTo(ptyCSIParam(params, 0, 1)-1, s.y)
	case 'A':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		s.moveTo(s.x, s.y-ptyCSIParam(params, 0, 1))
	case 'C':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		s.moveTo(s.x+ptyCSIParam(params, 0, 1), s.y)
	case 'J':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		mode := ptyCSIParam(params, 0, 0)
		if mode != 2 {
			return fmt.Errorf("unsupported erase-display mode %d", mode)
		}
		s.clear()
		if wasHome {
			s.fullRedraws++
		}
	case 'K':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		mode := ptyCSIParam(params, 0, 0)
		if mode != 0 && mode != 2 {
			return fmt.Errorf("unsupported erase-line mode %d", mode)
		}
		from := s.x
		if mode == 2 {
			from = 0
		}
		s.eraseRow(s.y, from, s.width-1)
	case 'X':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		count := min(ptyCSIParam(params, 0, 1), max(s.width-s.x, 0))
		s.eraseRow(s.y, s.x, s.x+count-1)
	case 'L':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		count := min(ptyCSIParam(params, 0, 1), max(s.height-s.y, 0))
		s.insertLines(count)
	default:
		return fmt.Errorf("unsupported terminal CSI final %q", final)
	}
	s.acceptedCSI[fmt.Sprintf("CSI %s%c", rawParams, final)] = struct{}{}
	return nil
}

func validatePTYSGRParams(raw string) error {
	if raw != "" {
		params, err := parsePTYCSIParams(raw, maxPTYCSIParams, false)
		if err != nil {
			return fmt.Errorf("invalid SGR params: %w", err)
		}
		if len(params) == 0 {
			return fmt.Errorf("invalid empty SGR params")
		}
	}

	// These are the exact SGR forms observed in both post-resize PTY slices.
	switch raw {
	case "", "1", "22", "30", "37", "37;40", "38;5;240", "38;5;240;27", "38;5;252", "39", "39;7", "40", "48;5;236":
		return nil
	default:
		return fmt.Errorf("unobserved SGR params %q", raw)
	}
}

func TestValidatePTYSGRParams(t *testing.T) {
	tests := []struct {
		name    string
		raw     string
		wantErr bool
	}{
		{name: "accept empty", raw: ""},
		{name: "accept bold", raw: "1"},
		{name: "accept normal intensity", raw: "22"},
		{name: "accept black foreground", raw: "30"},
		{name: "accept white foreground", raw: "37"},
		{name: "accept boxed fg/bg", raw: "37;40"},
		{name: "accept boxed background", raw: "48;5;236"},
		{name: "accept accent foreground", raw: "38;5;240"},
		{name: "accept accent foreground with alt", raw: "38;5;240;27"},
		{name: "accept border foreground", raw: "38;5;252"},
		{name: "accept reset foreground", raw: "39"},
		{name: "accept reset with reverse video", raw: "39;7"},
		{name: "accept background", raw: "40"},
		{name: "reject generalized background", raw: "48;5;235", wantErr: true},
		{name: "reject broadened background", raw: "48;5;236;1", wantErr: true},
		{name: "reject generalized foreground", raw: "38;5;241", wantErr: true},
		{name: "reject reordered form", raw: "40;37", wantErr: true},
		{name: "reject RGB syntax", raw: "38;2;1;2;3", wantErr: true},
		{name: "reject malformed separator", raw: "37;", wantErr: true},
		{name: "reject unobserved color", raw: "31", wantErr: true},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			err := validatePTYSGRParams(tc.raw)
			if tc.wantErr {
				if err == nil {
					t.Fatalf("validatePTYSGRParams(%q) = nil, want error", tc.raw)
				}
				return
			}
			if err != nil {
				t.Fatalf("validatePTYSGRParams(%q) = %v, want nil", tc.raw, err)
			}
		})
	}
}

func parsePTYCSIParams(raw string, maxFields int, allowEmpty bool) ([]int, error) {
	if raw == "" {
		return nil, nil
	}
	parts := strings.Split(raw, ";")
	if len(parts) > maxFields {
		return nil, fmt.Errorf("too many CSI params: got %d, max %d", len(parts), maxFields)
	}
	params := make([]int, len(parts))
	for index, part := range parts {
		if part == "" {
			if !allowEmpty {
				return nil, fmt.Errorf("empty CSI param %d", index+1)
			}
			params[index] = -1
			continue
		}
		for _, digit := range part {
			if digit < '0' || digit > '9' {
				return nil, fmt.Errorf("malformed CSI param %q", part)
			}
		}
		value, err := strconv.ParseUint(part, 10, 32)
		if err != nil || value > maxPTYCSIParam {
			return nil, fmt.Errorf("CSI param %q out of range", part)
		}
		params[index] = int(value)
	}
	return params, nil
}

func ptyCSIParam(params []int, index, fallback int) int {
	if index >= len(params) || params[index] < 0 || (params[index] == 0 && fallback == 1) {
		return fallback
	}
	return params[index]
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

func (s *ptyTerminalScreen) reverseIndex() {
	s.homePending = false
	if s.y > 0 {
		s.y--
		return
	}
	for row := s.height - 1; row > 0; row-- {
		copy(s.cells[row], s.cells[row-1])
	}
	if s.height > 0 {
		s.cells[0] = blankPTYRow(s.width)
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

func (s *ptyTerminalScreen) Cursor() (x, y int, visible bool) {
	return s.x, s.y, s.cursorVisible
}

func (s *ptyTerminalScreen) AcceptedCSI() []string {
	sequences := make([]string, 0, len(s.acceptedCSI))
	for sequence := range s.acceptedCSI {
		sequences = append(sequences, sequence)
	}
	sort.Strings(sequences)
	return sequences
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
