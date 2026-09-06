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
// Tea emits in the PTY tests. Unknown operations fail closed.
type ptyTerminalScreen struct {
	width, height int
	x, y          int
	top, bottom   int // scrolling region rows, inclusive
	cells         [][]rune
	pending       []byte
	cursorVisible bool
	insertMode    bool
	lastRune      rune
	fullRedraws   int
	acceptedCSI   map[string]struct{}
}

func newPTYTerminalScreen(width, height int) *ptyTerminalScreen {
	screen := &ptyTerminalScreen{width: width, height: height, bottom: height - 1, acceptedCSI: make(map[string]struct{})}
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

func TestPTYTerminalScreenLineEdits(t *testing.T) {
	tests := []struct {
		name  string
		input string
		want  string
	}{
		{name: "insert character shifts the tail right", input: "abcdef\x1b[6D\x1b[2@XY", want: "XYabcdef"},
		{name: "delete character shifts the tail left", input: "abcdef\x1b[6D\x1b[2P", want: "cdef"},
		{name: "repeat previous character", input: "ab\x1b[3b", want: "abbbb"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			screen := newPTYTerminalScreen(20, 1)
			if _, err := screen.Write([]byte(test.input)); err != nil {
				t.Fatal(err)
			}
			if got := strings.TrimSpace(screen.String()); got != test.want {
				t.Fatalf("screen = %q, want %q", got, test.want)
			}
		})
	}
}

func TestPTYTerminalScreenScrollRegion(t *testing.T) {
	steps := []struct {
		name  string
		input string
		want  string
	}{
		{name: "fill", input: "a\r\nb\r\nc\r\nd", want: "a|b|c|d"},
		{name: "line feed at region bottom scrolls the region", input: "\x1b[2;3r\x1b[3;1H\n", want: "a|c||d"},
		{name: "reverse index at region top scrolls the region down", input: "\x1b[2;1H\x1bM", want: "a||c|d"},
		{name: "delete line inside full region", input: "\x1b[1;4r\x1b[2;1H\x1b[M", want: "a|c|d|"},
		{name: "insert line inside full region", input: "\x1b[2;1H\x1b[L", want: "a||c|d"},
		{name: "scroll up", input: "\x1b[S", want: "|c|d|"},
		{name: "scroll down", input: "\x1b[T", want: "||c|d"},
	}
	screen := newPTYTerminalScreen(4, 4)
	for _, step := range steps {
		if _, err := screen.Write([]byte(step.input)); err != nil {
			t.Fatalf("%s: %v", step.name, err)
		}
		rows := strings.Split(screen.String(), "\n")
		for i := range rows {
			rows[i] = strings.TrimSpace(rows[i])
		}
		if got := strings.Join(rows, "|"); got != step.want {
			t.Fatalf("%s: screen = %q, want %q", step.name, got, step.want)
		}
	}
}

func TestPTYTerminalScreenInsertMode(t *testing.T) {
	screen := newPTYTerminalScreen(20, 1)
	if _, err := screen.Write([]byte("conTAIL\x1b[4D\x1b[4htext\x1b[4l.")); err != nil {
		t.Fatal(err)
	}
	if got := strings.TrimSpace(screen.String()); got != "context.AIL" {
		t.Fatalf("screen = %q, want inserted text followed by overwrite", got)
	}
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
		switch s.pending[1] {
		case 'M':
			s.reverseIndex()
			return 2, true, nil
		case ']':
			return s.consumeOSC()
		}
		return s.consumeCSI()
	}

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
	maxPTYOSCSequence = 4096
)

// consumeOSC skips an operating-system command (title, hyperlink, cursor
// color) terminated by BEL or ST. OSC never changes cell content.
func (s *ptyTerminalScreen) consumeOSC() (int, bool, error) {
	for i := 2; i < len(s.pending); i++ {
		switch {
		case s.pending[i] == '\a':
			return i + 1, true, nil
		case s.pending[i] == '\x1b' && i+1 < len(s.pending) && s.pending[i+1] == '\\':
			return i + 2, true, nil
		case s.pending[i] == '\x1b':
			return 0, false, fmt.Errorf("unterminated OSC before escape in %q", s.pending[:i+1])
		}
	}
	if len(s.pending) > maxPTYOSCSequence {
		return 0, false, fmt.Errorf("OSC sequence exceeds %d bytes", maxPTYOSCSequence)
	}
	return 0, false, nil
}

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
		case current == ' ':
			// DECSCUSR (CSI Ps SP q) sets the cursor shape and never changes cells.
			if index+1 >= len(s.pending) {
				return 0, false, nil
			}
			if s.pending[index+1] != 'q' {
				return 0, false, fmt.Errorf("unsupported CSI intermediate 0x20 before 0x%02x", s.pending[index+1])
			}
			sequence := string(s.pending[:index+2])
			if _, err := parsePTYCSIParams(string(s.pending[2:index]), 1, true); err != nil {
				return 0, false, fmt.Errorf("%w in %q", err, sequence)
			}
			s.acceptedCSI[fmt.Sprintf("CSI %s q", string(s.pending[2:index]))] = struct{}{}
			return index + 2, true, nil
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
	switch final {
	case 'm':
		if err := validatePTYSGRParams(rawParams); err != nil {
			return err
		}
	case 'h', 'l':
		switch rawParams {
		case "4":
			s.insertMode = final == 'h'
		case "?25":
			s.cursorVisible = final == 'h'
		default:
			return fmt.Errorf("unsupported terminal mode %q", rawParams)
		}
	case 'H', 'f':
		params, err := parsePTYCSIParams(rawParams, 2, true)
		if err != nil {
			return err
		}
		row := ptyCSIParam(params, 0, 1) - 1
		column := ptyCSIParam(params, 1, 1) - 1
		s.moveTo(column, row)
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
	case 'B':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		s.moveTo(s.x, s.y+ptyCSIParam(params, 0, 1))
	case 'C':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		s.moveTo(s.x+ptyCSIParam(params, 0, 1), s.y)
	case 'D':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		s.moveTo(s.x-ptyCSIParam(params, 0, 1), s.y)
	case 'J':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		mode := ptyCSIParam(params, 0, 0)
		switch mode {
		case 0:
			s.eraseRow(s.y, s.x, s.width-1)
			for row := s.y + 1; row < s.height; row++ {
				s.cells[row] = blankPTYRow(s.width)
			}
		case 2:
			s.clear()
		default:
			return fmt.Errorf("unsupported erase-display mode %d", mode)
		}
		// Bubble Tea's inline renderer issues erase-display immediately before
		// redrawing the live region from scratch, so every accepted erase is a
		// full-frame redraw boundary.
		s.fullRedraws++
	case 'K':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		from, to := s.x, s.width-1
		switch ptyCSIParam(params, 0, 0) {
		case 0:
		case 1:
			from, to = 0, s.x
		case 2:
			from = 0
		default:
			return fmt.Errorf("unsupported erase-line mode %s", rawParams)
		}
		s.eraseRow(s.y, from, to)
	case 'X':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		count := min(ptyCSIParam(params, 0, 1), max(s.width-s.x, 0))
		s.eraseRow(s.y, s.x, s.x+count-1)
	case 'L', 'M':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		if s.y >= s.top && s.y <= s.bottom {
			count := ptyCSIParam(params, 0, 1)
			if final == 'L' {
				count = -count
			}
			s.scrollRows(s.y, s.bottom, count)
		}
	case 'S', 'T':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		count := ptyCSIParam(params, 0, 1)
		if final == 'T' {
			count = -count
		}
		s.scrollRows(s.top, s.bottom, count)
	case 'r':
		params, err := parsePTYCSIParams(rawParams, 2, true)
		if err != nil {
			return err
		}
		top, bottom := ptyCSIParam(params, 0, 1)-1, ptyCSIParam(params, 1, s.height)-1
		if top < 0 || bottom >= s.height || top >= bottom {
			return fmt.Errorf("invalid scrolling region %d-%d for height %d", top+1, bottom+1, s.height)
		}
		s.top, s.bottom = top, bottom
		s.moveTo(0, 0)
	case '@', 'P':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		count := min(ptyCSIParam(params, 0, 1), max(s.width-s.x, 0))
		if count > 0 && s.y < s.height {
			row := s.cells[s.y]
			if final == '@' {
				copy(row[s.x+count:], row[s.x:])
				s.eraseRow(s.y, s.x, s.x+count-1)
			} else {
				copy(row[s.x:], row[s.x+count:])
				s.eraseRow(s.y, s.width-count, s.width-1)
			}
		}
	case 'b':
		params, err := parsePTYCSIParams(rawParams, 1, true)
		if err != nil {
			return err
		}
		if s.lastRune == 0 {
			return fmt.Errorf("repeat with no previous character")
		}
		for count := ptyCSIParam(params, 0, 1); count > 0; count-- {
			s.putRune(s.lastRune)
		}
	default:
		return fmt.Errorf("unsupported terminal CSI final %q", final)
	}
	s.acceptedCSI[fmt.Sprintf("CSI %s%c", rawParams, final)] = struct{}{}
	return nil
}

// validatePTYSGRParams accepts well-formed SGR parameter lists: attributes
// 0-9 and 21-29, colors 30-37, 39, 40-47, 49, 90-97, and 100-107, and the
// 38/48 extended forms `5;n` (n <= 255) and `2;r;g;b`. SGR never changes cell
// content, so only the shape is checked.
func validatePTYSGRParams(raw string) error {
	if raw == "" {
		return nil
	}
	params, err := parsePTYCSIParams(raw, maxPTYCSIParams, false)
	if err != nil {
		return fmt.Errorf("invalid SGR params: %w", err)
	}
	for i := 0; i < len(params); i++ {
		p := params[i]
		switch {
		case p == 38 || p == 48:
			rest := params[i+1:]
			switch {
			case len(rest) >= 2 && rest[0] == 5 && rest[1] <= 255:
				i += 2
			case len(rest) >= 4 && rest[0] == 2 && rest[1] <= 255 && rest[2] <= 255 && rest[3] <= 255:
				i += 4
			default:
				return fmt.Errorf("malformed extended color in SGR params %q", raw)
			}
		case p <= 9, p >= 21 && p <= 29, p >= 30 && p <= 37, p == 39, p >= 40 && p <= 47, p == 49, p >= 90 && p <= 97, p >= 100 && p <= 107:
		default:
			return fmt.Errorf("unsupported SGR attribute %d in %q", p, raw)
		}
	}
	return nil
}

func TestValidatePTYSGRParams(t *testing.T) {
	tests := []struct {
		name    string
		raw     string
		wantErr bool
	}{
		{name: "accept empty", raw: ""},
		{name: "accept reset", raw: "0"},
		{name: "accept bold", raw: "1"},
		{name: "accept normal intensity", raw: "22"},
		{name: "accept black foreground", raw: "30"},
		{name: "accept white foreground", raw: "37"},
		{name: "accept boxed fg/bg", raw: "37;40"},
		{name: "accept boxed background", raw: "48;5;236"},
		{name: "accept accent foreground", raw: "38;5;240"},
		{name: "accept accent foreground with alt", raw: "38;5;240;27"},
		{name: "accept accent foreground with cursor line background", raw: "38;5;240;40"},
		{name: "accept border foreground", raw: "38;5;252"},
		{name: "accept reset foreground", raw: "39"},
		{name: "accept reset with reverse video", raw: "39;7"},
		{name: "accept background", raw: "40"},
		{name: "accept any indexed color", raw: "48;5;237"},
		{name: "accept extended color followed by attribute", raw: "48;5;236;1"},
		{name: "accept reordered form", raw: "40;37"},
		{name: "accept RGB syntax", raw: "38;2;1;2;3"},
		{name: "accept bright colors", raw: "97;107"},
		{name: "reject malformed separator", raw: "37;", wantErr: true},
		{name: "reject truncated extended color", raw: "38;5", wantErr: true},
		{name: "reject out-of-range indexed color", raw: "48;5;256", wantErr: true},
		{name: "reject unknown extended color mode", raw: "38;7;1", wantErr: true},
		{name: "reject unknown attribute", raw: "999", wantErr: true},
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
	if s.insertMode {
		shift := min(width, s.width-s.x)
		copy(s.cells[s.y][s.x+shift:], s.cells[s.y][s.x:])
	}
	s.cells[s.y][s.x] = r
	for offset := 1; offset < width && s.x+offset < s.width; offset++ {
		s.cells[s.y][s.x+offset] = ' '
	}
	s.x += width
	s.lastRune = r
}

func (s *ptyTerminalScreen) lineFeed() {
	switch {
	case s.y == s.bottom:
		s.scrollRows(s.top, s.bottom, 1)
	case s.y < s.height-1:
		s.y++
	}
}

func (s *ptyTerminalScreen) clear() {
	for row := range s.cells {
		s.cells[row] = blankPTYRow(s.width)
	}
}

func (s *ptyTerminalScreen) reverseIndex() {
	switch {
	case s.y == s.top:
		s.scrollRows(s.top, s.bottom, -1)
	case s.y > 0:
		s.y--
	}
}

// scrollRows shifts rows from..to (inclusive) up by n (n > 0) or down by -n
// (n < 0) and blanks the vacated rows. IL, DL, SU, SD, LF at the region
// bottom, and RI at the region top all reduce to this.
func (s *ptyTerminalScreen) scrollRows(from, to, n int) {
	if from < 0 || to >= s.height || from > to || n == 0 {
		return
	}
	rows := s.cells[from : to+1]
	count := min(max(n, -n), len(rows))
	if n > 0 {
		copy(rows, rows[count:])
		for row := len(rows) - count; row < len(rows); row++ {
			rows[row] = blankPTYRow(s.width)
		}
		return
	}
	copy(rows[count:], rows)
	for row := 0; row < count; row++ {
		rows[row] = blankPTYRow(s.width)
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
