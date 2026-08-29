package agent

import (
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"path/filepath"
	"sort"
	"strings"
	"unicode"
	"unicode/utf8"

	"github.com/baiyuqing/otto/internal/model"
	"github.com/baiyuqing/otto/internal/session"
)

type fileToolCall struct {
	name      string
	arguments json.RawMessage
}

func appendCompactionFileBlocks(summary string, details session.CompactionDetails) (string, error) {
	if strings.TrimSpace(summary) == "" || !utf8.ValidString(summary) {
		return "", errors.New("complete compaction summary must be nonempty UTF-8")
	}
	complete := summary
	if suffix := compactionFileBlocks(details); suffix != "" {
		complete += "\n\n" + suffix
	}
	if !utf8.ValidString(complete) {
		return "", errors.New("complete compaction summary is not valid UTF-8")
	}
	if len(complete) > summaryMaximumBytes {
		return "", fmt.Errorf("complete compaction summary exceeds %d bytes", summaryMaximumBytes)
	}
	return complete, nil
}

func compactionFileBlocks(details session.CompactionDetails) string {
	var suffix strings.Builder
	writeBlock := func(tag string, paths []string) {
		if len(paths) == 0 {
			return
		}
		if suffix.Len() != 0 {
			suffix.WriteString("\n\n")
		}
		suffix.WriteByte('<')
		suffix.WriteString(tag)
		suffix.WriteString(">\n")
		for _, path := range paths {
			suffix.WriteString(path)
			suffix.WriteByte('\n')
		}
		suffix.WriteString("</")
		suffix.WriteString(tag)
		suffix.WriteByte('>')
	}
	writeBlock("read-files", details.ReadFiles)
	writeBlock("modified-files", details.ModifiedFiles)
	return suffix.String()
}

func stripCompactionFileBlocks(summary string, details session.CompactionDetails) string {
	suffix := compactionFileBlocks(details)
	if suffix == "" {
		return summary
	}
	return strings.TrimSuffix(summary, "\n\n"+suffix)
}

func deriveCompactionFileDetails(messages []model.Message, previous session.CompactionDetails) session.CompactionDetails {
	reads := make(map[string]struct{})
	modified := make(map[string]struct{})
	addPreviousDetailPaths(reads, previous.ReadFiles)
	addPreviousDetailPaths(modified, previous.ModifiedFiles)

	pending := make(map[string]fileToolCall)
	invalidIDs := make(map[string]struct{})
	for _, message := range messages {
		switch message.Role {
		case model.RoleAssistant:
			if len(pending) != 0 {
				clear(pending)
			}
			for _, block := range message.Blocks {
				if block.Type != model.BlockToolCall || block.ToolCallID == "" {
					continue
				}
				if _, duplicate := pending[block.ToolCallID]; duplicate {
					delete(pending, block.ToolCallID)
					invalidIDs[block.ToolCallID] = struct{}{}
					continue
				}
				if _, invalid := invalidIDs[block.ToolCallID]; invalid {
					continue
				}
				pending[block.ToolCallID] = fileToolCall{name: block.ToolName, arguments: append(json.RawMessage(nil), block.Arguments...)}
			}
		case model.RoleTool:
			for _, block := range message.Blocks {
				if block.Type != model.BlockToolResult {
					continue
				}
				call, paired := pending[block.ToolCallID]
				delete(pending, block.ToolCallID)
				if !paired || block.IsError || call.name != block.ToolName {
					continue
				}
				path, ok := filePathFromToolArguments(call.arguments)
				if !ok {
					continue
				}
				switch call.name {
				case "read":
					reads[path] = struct{}{}
				case "write", "edit":
					modified[path] = struct{}{}
				}
			}
		default:
			clear(pending)
		}
	}

	for path := range modified {
		delete(reads, path)
	}
	return boundCompactionFileDetails(reads, modified, previous)
}

func addPreviousDetailPaths(destination map[string]struct{}, paths []string) {
	for _, path := range paths {
		if normalized, ok := normalizeDetailPath(path); ok {
			destination[normalized] = struct{}{}
		}
	}
}

func filePathFromToolArguments(arguments json.RawMessage) (string, bool) {
	value, err := decodePairPreservingJSON(arguments)
	if err != nil {
		return "", false
	}
	object, ok := value.(jsonObject)
	if !ok {
		return "", false
	}
	path := ""
	found := false
	for _, member := range object {
		if member.key != "path" {
			continue
		}
		if found {
			return "", false
		}
		path, ok = member.value.(string)
		if !ok {
			return "", false
		}
		found = true
	}
	if !found {
		return "", false
	}
	return normalizeDetailPath(path)
}

func normalizeDetailPath(path string) (string, bool) {
	if path == "" || !utf8.ValidString(path) {
		return "", false
	}
	for _, character := range path {
		if unicode.IsControl(character) {
			return "", false
		}
	}
	cleaned := filepath.Clean(path)
	if cleaned == "" || !utf8.ValidString(cleaned) {
		return "", false
	}
	return cleaned, true
}

func boundCompactionFileDetails(readSet, modifiedSet map[string]struct{}, previous session.CompactionDetails) session.CompactionDetails {
	reads := sortedDetailPaths(readSet)
	modified := sortedDetailPaths(modifiedSet)
	result := session.CompactionDetails{
		OmittedReadFiles:     max(previous.OmittedReadFiles, 0),
		OmittedModifiedFiles: max(previous.OmittedModifiedFiles, 0),
	}
	remainingPaths := fileDetailsMaximumPaths
	remainingBytes := fileDetailsMaximumBytes
	for _, path := range modified {
		if remainingPaths == 0 || len(path) > remainingBytes {
			result.OmittedModifiedFiles = saturatingDetailCount(result.OmittedModifiedFiles, 1)
			continue
		}
		result.ModifiedFiles = append(result.ModifiedFiles, path)
		remainingPaths--
		remainingBytes -= len(path)
	}
	for _, path := range reads {
		if remainingPaths == 0 || len(path) > remainingBytes {
			result.OmittedReadFiles = saturatingDetailCount(result.OmittedReadFiles, 1)
			continue
		}
		result.ReadFiles = append(result.ReadFiles, path)
		remainingPaths--
		remainingBytes -= len(path)
	}
	return result
}

func sortedDetailPaths(paths map[string]struct{}) []string {
	result := make([]string, 0, len(paths))
	for path := range paths {
		result = append(result, path)
	}
	sort.Strings(result)
	return result
}

func saturatingDetailCount(total, delta int) int {
	if delta <= 0 {
		return max(total, 0)
	}
	if total < 0 {
		total = 0
	}
	if total > math.MaxInt-delta {
		return math.MaxInt
	}
	return total + delta
}
