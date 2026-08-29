package agent

import (
	"errors"
	"fmt"
	"strings"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/model"
)

func validateStructuredSummary(message model.Message) (string, error) {
	summary, err := validateSummaryMessage(message, summaryMaximumBytes)
	if err != nil {
		return "", err
	}
	if err := validateSummaryHeadings(summary); err != nil {
		return "", err
	}
	return summary, nil
}

func validateTurnSummary(message model.Message) (string, error) {
	summary, err := validateSummaryMessage(message, turnSummaryMaximumBytes)
	if err != nil {
		return "", err
	}
	if err := validateTurnSummaryHeadings(summary); err != nil {
		return "", err
	}
	return summary, nil
}

func validateSummaryMessage(message model.Message, maximumBytes int) (string, error) {
	if message.FinishReason == model.FinishToolCalls {
		return "", errors.New("compaction summary response attempted a tool call")
	}
	var text strings.Builder
	for _, block := range message.Blocks {
		if block.Type != model.BlockText {
			return "", errors.New("compaction summary response must contain text only and no tool calls")
		}
		text.WriteString(block.Text)
	}
	raw := text.String()
	if !utf8.ValidString(raw) {
		return "", errors.New("compaction summary response is not valid UTF-8")
	}
	summary := strings.TrimSpace(raw)
	if summary == "" {
		return "", errors.New("compaction summary response is empty")
	}
	if len(summary) > maximumBytes {
		return "", fmt.Errorf("compaction summary response exceeds %d bytes", maximumBytes)
	}
	return summary, nil
}

func validateSummaryHeadings(summary string) error {
	expected := 0
	fenceCharacter := byte(0)
	fenceLength := 0
	normalized := strings.ReplaceAll(summary, "\r\n", "\n")
	normalized = strings.ReplaceAll(normalized, "\r", "\n")
	for _, line := range strings.Split(normalized, "\n") {
		if marker, length, closing := summaryFenceMarker(line, fenceCharacter, fenceLength); marker != 0 {
			if fenceCharacter == 0 && !closing {
				fenceCharacter, fenceLength = marker, length
			} else if fenceCharacter == marker && closing {
				fenceCharacter, fenceLength = 0, 0
			}
			continue
		}
		if fenceCharacter != 0 || !isLevelTwoOrThreeHeading(line) {
			continue
		}
		if expected >= len(requiredSummaryHeadings) || line != requiredSummaryHeadings[expected] {
			return errors.New("compaction summary has an unexpected or out-of-order heading")
		}
		expected++
	}
	if expected != len(requiredSummaryHeadings) {
		return fmt.Errorf("compaction summary has %d of %d required headings", expected, len(requiredSummaryHeadings))
	}
	return nil
}

func validateTurnSummaryHeadings(summary string) error {
	fenceCharacter := byte(0)
	fenceLength := 0
	normalized := strings.ReplaceAll(summary, "\r\n", "\n")
	normalized = strings.ReplaceAll(normalized, "\r", "\n")
	for _, line := range strings.Split(normalized, "\n") {
		if marker, length, closing := summaryFenceMarker(line, fenceCharacter, fenceLength); marker != 0 {
			if fenceCharacter == 0 && !closing {
				fenceCharacter, fenceLength = marker, length
			} else if fenceCharacter == marker && closing {
				fenceCharacter, fenceLength = 0, 0
			}
			continue
		}
		if fenceCharacter == 0 && isLevelTwoOrThreeHeading(line) {
			return errors.New("turn compaction summary contains a level-2 or level-3 heading")
		}
	}
	return nil
}

func summaryFenceMarker(line string, active byte, activeLength int) (byte, int, bool) {
	trimmed := line
	spaces := 0
	for spaces < len(trimmed) && spaces < 4 && trimmed[spaces] == ' ' {
		spaces++
	}
	if spaces > 3 {
		return 0, 0, false
	}
	trimmed = trimmed[spaces:]
	if len(trimmed) < 3 || trimmed[0] != '`' && trimmed[0] != '~' {
		return 0, 0, false
	}
	marker := trimmed[0]
	length := 0
	for length < len(trimmed) && trimmed[length] == marker {
		length++
	}
	if length < 3 {
		return 0, 0, false
	}
	remainder := trimmed[length:]
	if active == 0 {
		if marker == '`' && strings.ContainsRune(remainder, '`') {
			return 0, 0, false
		}
		return marker, length, false
	}
	if marker == active && length >= activeLength && isFenceClosingRemainder(remainder) {
		return marker, length, true
	}
	return 0, 0, false
}

func isFenceClosingRemainder(remainder string) bool {
	for index := range len(remainder) {
		if remainder[index] != ' ' && remainder[index] != '\t' {
			return false
		}
	}
	return true
}

func isLevelTwoOrThreeHeading(line string) bool {
	spaces := 0
	for spaces < len(line) && spaces < 3 && line[spaces] == ' ' {
		spaces++
	}
	line = line[spaces:]
	return line == "##" || strings.HasPrefix(line, "## ") || strings.HasPrefix(line, "##\t") ||
		line == "###" || strings.HasPrefix(line, "### ") || strings.HasPrefix(line, "###\t")
}

func combineSummary(historical, turn string) (string, error) {
	validatedHistorical, err := validateStructuredSummary(model.Message{Blocks: []model.Block{{Type: model.BlockText, Text: historical}}})
	if err != nil {
		return "", err
	}
	validatedTurn, err := validateTurnSummary(model.Message{Blocks: []model.Block{{Type: model.BlockText, Text: turn}}})
	if err != nil {
		return "", err
	}
	combined := validatedHistorical + splitTurnSummarySeparator + validatedTurn
	if len(combined) > summaryMaximumBytes {
		return "", fmt.Errorf("combined compaction summary exceeds %d bytes", summaryMaximumBytes)
	}
	return combined, nil
}
