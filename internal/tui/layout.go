package tui

import (
	"fmt"
	"path/filepath"
	"strings"

	"charm.land/bubbles/v2/textarea"
	"charm.land/lipgloss/v2"
	"github.com/baiyuqing/otto/internal/app"
	otmodel "github.com/baiyuqing/otto/internal/model"
)

const (
	minTerminalWidth  = 40
	minTerminalHeight = 8
	minEditorHeight   = 1
	maxEditorHeight   = 6
	footerHeight      = 1
)

type layoutState struct {
	tooSmall         bool
	transcriptWidth  int
	transcriptHeight int
	editorHeight     int
	footerHeight     int
}

func calculateLayout(width, height int, editor textarea.Model) layoutState {
	layout := layoutState{
		transcriptWidth: max(0, width),
		editorHeight:    clamp(editorHeight(editor), minEditorHeight, maxEditorHeight),
		footerHeight:    footerHeight,
	}
	if width < minTerminalWidth || height < minTerminalHeight {
		layout.tooSmall = true
		layout.transcriptHeight = max(0, height)
		return layout
	}
	transcriptHeight := height - layout.editorHeight - layout.footerHeight
	if transcriptHeight <= 0 {
		layout.tooSmall = true
		layout.transcriptHeight = max(0, height)
		return layout
	}
	layout.transcriptHeight = transcriptHeight
	return layout
}

func editorHeight(editor textarea.Model) int {
	hardLines := strings.Count(editor.Value(), "\n") + 1
	return hardLines - 1 + max(1, editor.LineInfo().Height)
}

func smallTerminalView(width, height int) string {
	message := lipgloss.NewStyle().Bold(true).Render(
		fmt.Sprintf("terminal is too small — resize to at least %dx%d", minTerminalWidth, minTerminalHeight),
	)
	return lipgloss.Place(max(0, width), max(0, height), lipgloss.Center, lipgloss.Center, message)
}

func renderFooter(width int, info app.Info, usage otmodel.Usage) string {
	profileModel := strings.Trim(strings.Trim(info.Profile+"/"+info.Model, "/"), " ")
	if profileModel == "" {
		profileModel = "unknown/unknown"
	}

	fields := []string{profileModel}
	if workspace := footerWorkspace(info.Workspace); workspace != "" && width >= 72 {
		fields = append([]string{workspace}, fields...)
	}
	if width >= 48 {
		fields = append(fields, fmt.Sprintf("tokens %d/%d", max(0, usage.InputTokens), max(0, usage.OutputTokens)))
	}
	if info.SessionID != "" && width >= 60 {
		fields = append(fields, info.SessionID)
	}

	for len(fields) > 1 && lipgloss.Width(strings.Join(fields, " | ")) > max(0, width) {
		fields = fields[:len(fields)-1]
	}

	footer := strings.Join(fields, " | ")
	return lipgloss.NewStyle().Width(max(0, width)).Render(footer)
}

func footerWorkspace(workspace string) string {
	if workspace == "" {
		return ""
	}
	base := filepath.Base(workspace)
	if base == "." || base == string(filepath.Separator) {
		return workspace
	}
	return base
}

func renderOverlay(width, height int, content string) string {
	box := lipgloss.NewStyle().Border(lipgloss.RoundedBorder()).Padding(0, 1).Render(content)
	return lipgloss.Place(max(0, width), max(0, height), lipgloss.Center, lipgloss.Center, box)
}

func helpOverlayContent() string {
	return strings.Join([]string{
		"Help",
		"",
		"Enter submit",
		"Alt+Enter newline",
		"Ctrl+O toggle tool output",
		"PgUp/PgDn scroll",
		"Esc cancel or close overlay",
		"/session show session details",
		"/new new session",
		"/exit quit",
	}, "\n")
}

func sessionOverlayContent(info app.Info) string {
	lines := []string{"Session", ""}
	appendField := func(name, value string) {
		if value == "" {
			return
		}
		lines = append(lines, fmt.Sprintf("%s: %s", name, value))
	}
	appendField("ID", info.SessionID)
	appendField("Path", info.SessionPath)
	appendField("Provider", info.Provider)
	appendField("Profile", info.Profile)
	appendField("Model", info.Model)
	return strings.Join(lines, "\n")
}

func clamp(value, low, high int) int {
	if high < low {
		low, high = high, low
	}
	if value < low {
		return low
	}
	if value > high {
		return high
	}
	return value
}
