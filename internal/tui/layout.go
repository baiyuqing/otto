package tui

import (
	"fmt"
	"math/big"
	"path/filepath"
	"strings"

	"charm.land/bubbles/v2/textarea"
	"charm.land/lipgloss/v2"
	"github.com/baiyuqing/otto/internal/app"
	otmodel "github.com/baiyuqing/otto/internal/model"
	"github.com/charmbracelet/x/ansi"
)

const (
	minTerminalWidth  = 40
	minTerminalHeight = 8
	minEditorHeight   = 2
	maxEditorHeight   = 6
	footerHeight      = 1
	editorSpacing     = 1
	// inputBoxThreshold is the terminal height at which the composer gains its
	// bordered panel. Below it the editor stays compact so small terminals keep
	// room for the transcript and the command-suggestion panel.
	inputBoxThreshold = 12
	inputBoxBorder    = 2
	inputBoxLabel     = 1
	inputBoxPadding   = 1
)

type layoutState struct {
	tooSmall         bool
	transcriptWidth  int
	transcriptHeight int
	editorHeight     int
	suggestionHeight int
	footerHeight     int
	editorSpacing    int
	inputBoxed       bool
	inputBoxHeight   int
}

func calculateLayout(width, height int, editor textarea.Model, requestedSuggestionHeight int) layoutState {
	layout := layoutState{
		transcriptWidth: max(0, width),
		editorHeight:    clamp(editorHeight(editor), minEditorHeight, maxEditorHeight),
		footerHeight:    footerHeight,
		editorSpacing:   editorSpacing,
		inputBoxed:      height >= inputBoxThreshold,
	}
	if layout.inputBoxed {
		// The box's top border separates it from the transcript, so the extra
		// blank row is not needed and would waste space.
		layout.editorSpacing = 0
		layout.inputBoxHeight = layout.editorHeight + inputBoxBorder + inputBoxLabel
	} else {
		layout.inputBoxHeight = layout.editorHeight
	}
	if width < minTerminalWidth || height < minTerminalHeight {
		layout.tooSmall = true
		layout.transcriptHeight = max(0, height)
		return layout
	}
	availableHeight := height - layout.inputBoxHeight - layout.footerHeight - layout.editorSpacing
	layout.suggestionHeight = min(max(0, requestedSuggestionHeight), max(0, availableHeight-1))
	transcriptHeight := availableHeight - layout.suggestionHeight
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
	width = max(0, width)
	height = max(0, height)
	message := fmt.Sprintf("terminal is too small — resize to at least %dx%d", minTerminalWidth, minTerminalHeight)
	message = wrapAndClip(message, width, height)
	message = lipgloss.NewStyle().Bold(true).MaxWidth(width).MaxHeight(height).Render(message)
	return fitToBounds(lipgloss.Place(width, height, lipgloss.Center, lipgloss.Center, message), width, height)
}

func renderFooter(width int, info app.Info, usage otmodel.Usage, status string) string {
	width = max(0, width)
	profileModel := strings.Trim(strings.Trim(escapeSingleLineText(info.Profile)+"/"+escapeSingleLineText(info.Model), "/"), " ")
	if profileModel == "" {
		profileModel = "unknown/unknown"
	}

	status = escapeSingleLineText(status)
	badge := info.Sandbox.Badge()
	workspace := ""
	if width >= 72 {
		workspace = escapeSingleLineText(footerWorkspace(info.Workspace))
	}
	usageField := ""
	contextField := ""
	if width >= 48 {
		if usage.CachedInputTokens > 0 {
			usageField = fmt.Sprintf("tokens %s/%s (cached %s)", formatFooterTokenCount(usage.InputTokens), formatFooterTokenCount(usage.OutputTokens), formatFooterTokenCount(usage.CachedInputTokens))
		} else {
			usageField = fmt.Sprintf("tokens %s/%s", formatFooterTokenCount(usage.InputTokens), formatFooterTokenCount(usage.OutputTokens))
		}
		contextField = footerContextField(info)
	}
	sessionID := ""
	if info.SessionID != "" && width >= 60 {
		sessionID = escapeSingleLineText(info.SessionID)
	}

	optional := map[string]string{
		"workspace": workspace,
		"usage":     usageField,
		"context":   contextField,
		"session":   sessionID,
	}
	buildFields := func() []string {
		fields := make([]string, 0, 7)
		if status != "" {
			fields = append(fields, status)
		}
		if optional["workspace"] != "" {
			fields = append(fields, optional["workspace"])
		}
		fields = append(fields, profileModel, badge)
		if optional["usage"] != "" {
			fields = append(fields, optional["usage"])
		}
		if optional["context"] != "" {
			fields = append(fields, optional["context"])
		}
		if optional["session"] != "" {
			fields = append(fields, optional["session"])
		}
		return fields
	}

	fields := buildFields()
	for _, name := range []string{"session", "workspace", "context", "usage"} {
		if lipgloss.Width(strings.Join(fields, " | ")) <= width {
			break
		}
		if optional[name] == "" {
			continue
		}
		optional[name] = ""
		fields = buildFields()
	}

	footer := strings.Join(fields, " | ")
	if lipgloss.Width(footer) > width {
		footer = renderFooterCore(width, status, profileModel, badge)
	}
	return lipgloss.NewStyle().Width(width).MaxWidth(width).MaxHeight(1).Render(footer)
}

func renderFooterCore(width int, status, profileModel, badge string) string {
	if width <= 0 {
		return ""
	}
	const separator = " | "
	badgeWidth := ansi.StringWidth(badge)
	if badgeWidth >= width {
		return ansi.Truncate(badge, width, "")
	}
	available := width - badgeWidth - ansi.StringWidth(separator)
	if available <= 0 {
		return badge
	}

	left := profileModel
	if status != "" {
		withProfile := status + separator + profileModel
		if ansi.StringWidth(withProfile) <= available {
			left = withProfile
		} else {
			left = status
		}
	}
	if ansi.StringWidth(left) > available {
		left = ansi.Truncate(left, available, "…")
	}
	if left == "" {
		return badge
	}
	return left + separator + badge
}

func footerContextField(info app.Info) string {
	if info.ContextWindow <= 0 {
		return ""
	}
	if info.ContextInputTokensPresent {
		return "ctx " + formatFooterContextPercentage(info.ContextInputTokens, info.ContextWindow)
	}
	if info.ContextInputTokensPending {
		return "ctx ?%"
	}
	return ""
}

func formatFooterContextPercentage(inputTokens, contextWindow int) string {
	if inputTokens < 0 {
		inputTokens = 0
	}
	if contextWindow <= 0 {
		return "0.0%"
	}
	numerator := new(big.Int).Mul(big.NewInt(int64(inputTokens)), big.NewInt(1000))
	denominator := big.NewInt(int64(contextWindow))
	numerator.Add(numerator, new(big.Int).Rsh(new(big.Int).Set(denominator), 1))
	tenths := new(big.Int).Quo(numerator, denominator).String()
	if len(tenths) == 1 {
		return "0." + tenths + "%"
	}
	return tenths[:len(tenths)-1] + "." + tenths[len(tenths)-1:] + "%"
}

func formatFooterTokenCount(tokens int) string {
	if tokens <= 0 {
		return "0"
	}
	count := uint64(tokens)
	if count < 1000 {
		return fmt.Sprintf("%d", count)
	}
	if count < 1_000_000 {
		return formatFooterTokenCountUnit(count, 1_000, "k", "M", false)
	}
	if count < 1_000_000_000 {
		return formatFooterTokenCountUnit(count, 1_000_000, "M", "B", false)
	}
	return formatFooterTokenCountUnit(count, 1_000_000_000, "B", "", true)
}

func formatFooterTokenCountUnit(count, divisor uint64, suffix, nextSuffix string, largest bool) string {
	whole := count / divisor
	rem := count % divisor
	tenths := (rem*10 + divisor/2) / divisor
	whole += tenths / 10
	tenths %= 10
	if !largest && whole >= 1000 {
		return "1" + nextSuffix
	}
	if tenths == 0 {
		return fmt.Sprintf("%d%s", whole, suffix)
	}
	return fmt.Sprintf("%d.%d%s", whole, tenths, suffix)
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

func renderCommandSuggestions(width int, suggestions []slashCommand, selected, height int) string {
	if width <= 0 || height <= 0 || len(suggestions) == 0 {
		return ""
	}
	selected = clamp(selected, 0, len(suggestions)-1)
	start := 0
	if selected >= height {
		start = selected - height + 1
	}
	end := min(len(suggestions), start+height)
	lines := make([]string, 0, end-start)
	for index := start; index < end; index++ {
		marker := "  "
		if index == selected {
			marker = "> "
		}
		line := fmt.Sprintf("%s%-10s %s", marker, suggestions[index].Name, suggestions[index].Description)
		lines = append(lines, lipgloss.NewStyle().Width(max(0, width)).MaxWidth(max(0, width)).MaxHeight(1).Render(line))
	}
	if start == 0 && end+1 == len(suggestions) && len(lines) > 0 {
		lastVisible := suggestions[end-1]
		last := suggestions[len(suggestions)-1]
		marker := "  "
		if selected == end-1 {
			marker = "> "
		}
		combined := fmt.Sprintf("%s%s %s | %s %s", marker, lastVisible.Name, lastVisible.Description, last.Name, last.Description)
		if ansi.StringWidth(combined) <= width {
			lines[len(lines)-1] = lipgloss.NewStyle().Width(width).MaxWidth(width).MaxHeight(1).Render(combined)
		}
	}
	return strings.Join(lines, "\n")
}

func renderOverlay(width, height int, content string) string {
	width = max(0, width)
	height = max(0, height)
	if width < 4 || height < 3 {
		return fitToBounds(content, width, height)
	}
	innerWidth, innerHeight := overlayContentBounds(width, height)
	content = truncateAndClipLines(content, innerWidth, innerHeight)
	box := lipgloss.NewStyle().
		Border(lipgloss.RoundedBorder()).
		Padding(0, 1).
		MaxWidth(width).
		MaxHeight(height).
		Render(content)
	return fitToBounds(lipgloss.Place(width, height, lipgloss.Center, lipgloss.Center, box), width, height)
}

func overlayContentBounds(width, height int) (int, int) {
	width = max(0, width)
	height = max(0, height)
	if width < 4 || height < 3 {
		return width, height
	}
	// The overlay has a one-cell border and one cell of horizontal padding
	// on each side. Its border consumes the two vertical cells.
	return width - 4, height - 2
}

func sandboxOverlayLines(innerWidth int, info app.SandboxInfo) []string {
	if innerWidth <= 0 {
		return nil
	}
	return strings.Split(ansi.Wrap("Sandbox: "+info.Summary(), innerWidth, ""), "\n")
}

func helpOverlayContent(width, height int, info app.SandboxInfo) string {
	innerWidth, innerHeight := overlayContentBounds(width, height)
	sandboxLines := sandboxOverlayLines(innerWidth, info)
	full := []string{"Help (? or /help)"}
	full = append(full, sandboxLines...)
	full = append(full,
		"",
		"Enter submit",
		"Shift+Enter or Alt+Enter newline",
		"Ctrl+O toggle details",
		"Shift+drag select terminal text",
		"PgUp/PgDn scroll",
		"Home/End transcript top/bottom",
		"Esc cancel or close overlay",
		"Ctrl+C cancel, clear, then quit",
	)
	for _, command := range slashCommands {
		full = append(full, command.Name+" "+command.Description)
	}
	if len(full) <= innerHeight {
		return truncateAndClipLines(strings.Join(full, "\n"), innerWidth, innerHeight)
	}

	compactDetails := []string{
		"Help ? /help Enter Shift+Enter",
		"Alt+Enter Ctrl+O PgUp/PgDn Home/End",
		"Esc Ctrl+C /session /new /exit",
		"/resume /compact",
	}
	detailLimit := max(0, innerHeight-len(sandboxLines))
	compactDetails = compactDetails[:min(len(compactDetails), detailLimit)]
	compact := append(compactDetails, sandboxLines...)
	return truncateAndClipLines(strings.Join(compact, "\n"), innerWidth, innerHeight)
}

func sessionOverlayContent(width, height int, info app.Info) string {
	innerWidth, innerHeight := overlayContentBounds(width, height)
	lines := []string{"Session"}
	lines = append(lines, sandboxOverlayLines(innerWidth, info.Sandbox)...)
	if reason := info.Sandbox.ReasonCode(); reason != "" {
		lines = append(lines, "Sandbox reason: "+reason)
	}
	appendField := func(name, value string) {
		if value == "" {
			return
		}
		lines = append(lines, fmt.Sprintf("%s: %s", name, escapeSingleLineText(value)))
	}
	appendField("ID", info.SessionID)
	appendField("Path", info.SessionPath)
	appendField("Provider", info.Provider)
	appendField("Profile", info.Profile)
	appendField("Model", info.Model)
	return truncateAndClipLines(strings.Join(lines, "\n"), innerWidth, innerHeight)
}

func wrapAndClip(content string, width, height int) string {
	if width <= 0 || height <= 0 || content == "" {
		return ""
	}
	wrappedLines := make([]string, 0, min(height, strings.Count(content, "\n")+1))
	for _, line := range strings.Split(content, "\n") {
		wrapped := ansi.Wrap(line, width, "")
		wrappedLines = append(wrappedLines, strings.Split(wrapped, "\n")...)
		if len(wrappedLines) >= height {
			wrappedLines = wrappedLines[:height]
			break
		}
	}
	return strings.Join(wrappedLines, "\n")
}

func truncateAndClipLines(content string, width, height int) string {
	if width <= 0 || height <= 0 || content == "" {
		return ""
	}
	lines := strings.Split(content, "\n")
	if len(lines) > height {
		lines = lines[:height]
	}
	for i := range lines {
		lines[i] = ansi.Truncate(lines[i], width, "")
	}
	return strings.Join(lines, "\n")
}

func resumeVisibleRows(width, height int) int {
	_ = width
	// A bounded picker reserves two border rows, one title row, and one help row.
	return max(1, height-4)
}

func resumeVisibleRange(sessionCount, selected, visibleRows int) (int, int) {
	if sessionCount <= 0 {
		return 0, 0
	}
	selected = clamp(selected, 0, sessionCount-1)
	visibleRows = clamp(visibleRows, 1, sessionCount)
	start := (selected / visibleRows) * visibleRows
	end := min(sessionCount, start+visibleRows)
	return start, end
}

func clipSingleLineText(text string, width int) string {
	if width <= 0 || text == "" {
		return ""
	}
	safe := escapeSingleLineText(text)
	if ansi.StringWidth(safe) <= width {
		return safe
	}
	return ansi.Truncate(safe, width, "…")
}

func fitToBounds(content string, width, height int) string {
	if width <= 0 || height <= 0 || content == "" {
		return ""
	}
	return lipgloss.NewStyle().MaxWidth(width).MaxHeight(height).Render(content)
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
