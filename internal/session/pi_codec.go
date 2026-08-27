package session

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
)

const piCodecBufferBytes = 64 << 10

type readerWithLen interface {
	Len() int
}

func decodePiFile(reader io.Reader) (piFile, []Warning, error) {
	if sized, ok := reader.(readerWithLen); ok && sized.Len() > maxSessionFileBytes {
		return piFile{}, nil, sizeError(ErrSessionFileTooLarge, maxSessionFileBytes)
	}

	buffered := bufio.NewReaderSize(reader, piCodecBufferBytes)
	var (
		decoded    piFile
		record     []byte
		totalBytes int
		lineNumber int
	)

	for {
		fragment, readErr := buffered.ReadSlice('\n')
		if len(fragment) > 0 {
			if len(fragment) > maxSessionFileBytes-totalBytes {
				return piFile{}, nil, sizeError(ErrSessionFileTooLarge, maxSessionFileBytes)
			}
			totalBytes += len(fragment)

			hasLF := fragment[len(fragment)-1] == '\n'
			content := fragment
			if hasLF {
				content = fragment[:len(fragment)-1]
			}
			if len(content) > maxSessionEntryBytes-len(record) {
				return piFile{}, nil, sizeError(ErrSessionEntryTooLarge, maxSessionEntryBytes)
			}
			record = append(record, content...)

			if hasLF {
				lineNumber++
				if err := decodePiFileRecord(&decoded, lineNumber, record); err != nil {
					return piFile{}, nil, err
				}
				record = nil
			}
		}

		switch {
		case readErr == nil:
			continue
		case errors.Is(readErr, bufio.ErrBufferFull):
			continue
		case errors.Is(readErr, io.EOF):
			if len(record) > 0 {
				lineNumber++
				if err := decodePiFileRecord(&decoded, lineNumber, record); err != nil {
					return piFile{}, nil, err
				}
			}
			if lineNumber == 0 {
				return piFile{}, nil, fmt.Errorf("%w: session file is empty", ErrInvalidSession)
			}
			return decoded, nil, nil
		default:
			return piFile{}, nil, fmt.Errorf("read Pi session: %w", readErr)
		}
	}
}

func decodePiFileRecord(decoded *piFile, lineNumber int, record []byte) error {
	if len(record) == 0 {
		return fmt.Errorf("%w: line %d is empty", ErrInvalidSession, lineNumber)
	}
	if lineNumber == 1 {
		header, err := decodePiHeader(record)
		if err != nil {
			return fmt.Errorf("session line 1: %w", err)
		}
		decoded.Header = header
		return nil
	}
	entry, err := decodePiEntry(record)
	if err != nil {
		return fmt.Errorf("session line %d: %w", lineNumber, err)
	}
	decoded.Entries = append(decoded.Entries, entry)
	return nil
}

func decodePiHeader(raw []byte) (piHeader, error) {
	object, err := decodeObject(raw, "session header")
	if err != nil {
		return piHeader{}, err
	}
	typeName, err := requiredString(object, "type", "session header.type")
	if err != nil {
		return piHeader{}, err
	}
	if typeName != "session" {
		return piHeader{}, fmt.Errorf("%w: first record is not a Pi session header", ErrUnsupportedSessionFormat)
	}

	versionRaw, ok := object["version"]
	if !ok {
		return piHeader{}, fmt.Errorf("%w: Pi session version is missing", ErrUnsupportedSessionFormat)
	}
	var version int
	if isJSONNull(versionRaw) || json.Unmarshal(versionRaw, &version) != nil {
		return piHeader{}, invalidField("session header.version", "an integer")
	}
	if version != PiSessionVersion {
		return piHeader{}, fmt.Errorf("%w: Pi session version is not supported", ErrUnsupportedSessionFormat)
	}

	id, err := requiredString(object, "id", "session header.id")
	if err != nil {
		return piHeader{}, err
	}
	timestamp, err := requiredString(object, "timestamp", "session header.timestamp")
	if err != nil {
		return piHeader{}, err
	}
	cwd, err := requiredString(object, "cwd", "session header.cwd")
	if err != nil {
		return piHeader{}, err
	}
	parentSession, err := optionalString(object, "parentSession", "session header.parentSession", false)
	if err != nil {
		return piHeader{}, err
	}

	return piHeader{
		Type:          typeName,
		Version:       version,
		ID:            id,
		Timestamp:     timestamp,
		CWD:           cwd,
		ParentSession: parentSession,
		Raw:           cloneRaw(raw),
	}, nil
}

func decodePiEntry(raw []byte) (piEntry, error) {
	object, err := decodeObject(raw, "session entry")
	if err != nil {
		return piEntry{}, err
	}
	typeName, err := requiredString(object, "type", "session entry.type")
	if err != nil {
		return piEntry{}, err
	}
	id, err := requiredString(object, "id", "session entry.id")
	if err != nil {
		return piEntry{}, err
	}
	parentID, err := requiredNullableString(object, "parentId", "session entry.parentId")
	if err != nil {
		return piEntry{}, err
	}
	timestamp, err := requiredString(object, "timestamp", "session entry.timestamp")
	if err != nil {
		return piEntry{}, err
	}

	entry := piEntry{
		piEntryBase: piEntryBase{Type: typeName, ID: id, ParentID: parentID, Timestamp: timestamp},
		Raw:         cloneRaw(raw),
	}

	switch typeName {
	case "message":
		messageRaw, err := requiredObjectRaw(object, "message", "message entry.message")
		if err != nil {
			return piEntry{}, err
		}
		entry.Message, err = decodePiMessage(messageRaw, "message entry.message")
		if err != nil {
			return piEntry{}, err
		}
	case "model_change":
		entry.ModelChange, err = decodePiModelChange(object)
	case "thinking_level_change":
		entry.ThinkingLevelChange, err = decodePiThinkingLevelChange(object)
	case "compaction":
		entry.Compaction, err = decodePiCompaction(object)
	case "branch_summary":
		entry.BranchSummary, err = decodePiBranchSummary(object)
	case "custom":
		entry.Custom, err = decodePiCustom(object)
	case "custom_message":
		entry.CustomMessage, err = decodePiCustomMessage(object)
	case "label":
		entry.Label, err = decodePiLabel(object)
	case "session_info":
		entry.SessionInfo, err = decodePiSessionInfo(object)
	}
	if err != nil {
		return piEntry{}, err
	}
	return entry, nil
}

func decodePiMessage(raw []byte, path string) (*piMessage, error) {
	object, err := decodeObject(raw, path)
	if err != nil {
		return nil, err
	}
	role, err := requiredString(object, "role", path+".role")
	if err != nil {
		return nil, err
	}
	timestamp, err := requiredInt64(object, "timestamp", path+".timestamp")
	if err != nil {
		return nil, err
	}

	var message piMessage
	if err := json.Unmarshal(raw, &message); err != nil {
		return nil, fmt.Errorf("%w: %s contains a field with an invalid shape", ErrInvalidSession, path)
	}
	message.Role = role
	message.Timestamp = timestamp

	switch role {
	case "user":
		message.ContentText, message.ContentBlocks, err = decodeContentField(object, "content", path+".content", true)
	case "assistant":
		err = validateAssistantMessage(object, &message, path)
	case "toolResult":
		err = validateToolResultMessage(object, &message, path)
	case "bashExecution":
		err = validateBashExecutionMessage(object, path)
	case "custom":
		err = validateCustomAgentMessage(object, &message, path)
	case "branchSummary":
		_, err = requiredString(object, "summary", path+".summary")
		if err == nil {
			_, err = requiredString(object, "fromId", path+".fromId")
		}
	case "compactionSummary":
		_, err = requiredString(object, "summary", path+".summary")
		if err == nil {
			_, err = requiredInt64(object, "tokensBefore", path+".tokensBefore")
		}
	}
	if err != nil {
		return nil, err
	}
	return &message, nil
}

func validateAssistantMessage(object map[string]json.RawMessage, message *piMessage, path string) error {
	var err error
	message.ContentText, message.ContentBlocks, err = decodeContentField(object, "content", path+".content", false)
	if err != nil {
		return err
	}
	for _, field := range []string{"api", "provider", "model", "stopReason"} {
		if _, err := requiredString(object, field, path+"."+field); err != nil {
			return err
		}
	}
	usageRaw, err := requiredObjectRaw(object, "usage", path+".usage")
	if err != nil {
		return err
	}
	message.Usage, err = decodePiUsage(usageRaw, path+".usage")
	if err != nil {
		return err
	}
	for _, field := range []string{"responseModel", "responseId", "errorMessage", "rawStopReason"} {
		if _, err := optionalString(object, field, path+"."+field, false); err != nil {
			return err
		}
	}
	if err := validateOptionalBool(object, "endTurn", path+".endTurn"); err != nil {
		return err
	}
	if raw, ok := object["deferred"]; ok {
		if err := validateDeferredHandle(raw, path+".deferred"); err != nil {
			return err
		}
	}
	if raw, ok := object["diagnostics"]; ok {
		if err := validateDiagnostics(raw, path+".diagnostics"); err != nil {
			return err
		}
	}
	return nil
}

func validateToolResultMessage(object map[string]json.RawMessage, message *piMessage, path string) error {
	for _, field := range []string{"toolCallId", "toolName"} {
		if _, err := requiredString(object, field, path+"."+field); err != nil {
			return err
		}
	}
	var err error
	message.ContentText, message.ContentBlocks, err = decodeContentField(object, "content", path+".content", false)
	if err != nil {
		return err
	}
	if _, err := requiredBool(object, "isError", path+".isError"); err != nil {
		return err
	}
	if raw, ok := object["usage"]; ok {
		message.Usage, err = decodePiUsage(raw, path+".usage")
		if err != nil {
			return err
		}
	}
	if raw, ok := object["addedToolNames"]; ok {
		var names []string
		if !isJSONArray(raw) || json.Unmarshal(raw, &names) != nil {
			return invalidField(path+".addedToolNames", "an array of strings")
		}
	}
	return nil
}

func validateBashExecutionMessage(object map[string]json.RawMessage, path string) error {
	for _, field := range []string{"command", "output"} {
		if _, err := requiredString(object, field, path+"."+field); err != nil {
			return err
		}
	}
	for _, field := range []string{"cancelled", "truncated"} {
		if _, err := requiredBool(object, field, path+"."+field); err != nil {
			return err
		}
	}
	if raw, ok := object["exitCode"]; ok && !isJSONNull(raw) {
		var exitCode int
		if json.Unmarshal(raw, &exitCode) != nil {
			return invalidField(path+".exitCode", "an integer or null")
		}
	}
	if _, err := optionalString(object, "fullOutputPath", path+".fullOutputPath", false); err != nil {
		return err
	}
	return validateOptionalBool(object, "excludeFromContext", path+".excludeFromContext")
}

func validateCustomAgentMessage(object map[string]json.RawMessage, message *piMessage, path string) error {
	if _, err := requiredString(object, "customType", path+".customType"); err != nil {
		return err
	}
	var err error
	message.ContentText, message.ContentBlocks, err = decodeContentField(object, "content", path+".content", true)
	if err != nil {
		return err
	}
	_, err = requiredBool(object, "display", path+".display")
	return err
}

func decodePiUsage(raw []byte, path string) (*piUsage, error) {
	object, err := decodeObject(raw, path)
	if err != nil {
		return nil, err
	}
	var usage piUsage
	if err := json.Unmarshal(raw, &usage); err != nil {
		return nil, fmt.Errorf("%w: %s contains a field with an invalid shape", ErrInvalidSession, path)
	}
	for _, field := range []string{"input", "output", "cacheRead", "cacheWrite", "totalTokens"} {
		if _, err := requiredInt64(object, field, path+"."+field); err != nil {
			return nil, err
		}
	}
	for _, field := range []string{"cacheWrite1h", "reasoning"} {
		if raw, ok := object[field]; ok {
			var value int64
			if isJSONNull(raw) || json.Unmarshal(raw, &value) != nil {
				return nil, invalidField(path+"."+field, "an integer")
			}
		}
	}
	costRaw, err := requiredObjectRaw(object, "cost", path+".cost")
	if err != nil {
		return nil, err
	}
	cost, err := decodePiCost(costRaw, path+".cost")
	if err != nil {
		return nil, err
	}
	usage.Cost = cost
	return &usage, nil
}

func decodePiCost(raw []byte, path string) (piCost, error) {
	object, err := decodeObject(raw, path)
	if err != nil {
		return piCost{}, err
	}
	var cost piCost
	if err := json.Unmarshal(raw, &cost); err != nil {
		return piCost{}, fmt.Errorf("%w: %s contains a field with an invalid shape", ErrInvalidSession, path)
	}
	for _, field := range []string{"input", "output", "cacheRead", "cacheWrite", "total"} {
		if _, err := requiredFloat64(object, field, path+"."+field); err != nil {
			return piCost{}, err
		}
	}
	return cost, nil
}

func decodePiModelChange(object map[string]json.RawMessage) (*piModelChange, error) {
	provider, err := requiredString(object, "provider", "model_change.provider")
	if err != nil {
		return nil, err
	}
	modelID, err := requiredString(object, "modelId", "model_change.modelId")
	if err != nil {
		return nil, err
	}
	return &piModelChange{Provider: provider, ModelID: modelID}, nil
}

func decodePiThinkingLevelChange(object map[string]json.RawMessage) (*piThinkingLevelChange, error) {
	level, err := requiredString(object, "thinkingLevel", "thinking_level_change.thinkingLevel")
	if err != nil {
		return nil, err
	}
	return &piThinkingLevelChange{ThinkingLevel: level}, nil
}

func decodePiCompaction(object map[string]json.RawMessage) (*piCompaction, error) {
	summary, err := requiredString(object, "summary", "compaction.summary")
	if err != nil {
		return nil, err
	}
	tokensBefore, err := requiredInt64(object, "tokensBefore", "compaction.tokensBefore")
	if err != nil {
		return nil, err
	}
	firstKeptEntryID, err := optionalString(object, "firstKeptEntryId", "compaction.firstKeptEntryId", false)
	if err != nil {
		return nil, err
	}

	compaction := &piCompaction{Summary: summary, FirstKeptEntryID: firstKeptEntryID, TokensBefore: tokensBefore}
	if raw, ok := object["details"]; ok {
		compaction.Details = cloneRaw(raw)
	}
	if raw, ok := object["fromHook"]; ok {
		value, err := decodeBool(raw, "compaction.fromHook")
		if err != nil {
			return nil, err
		}
		compaction.FromHook = &value
	}
	if raw, ok := object["usage"]; ok {
		compaction.Usage, err = decodePiUsage(raw, "compaction.usage")
		if err != nil {
			return nil, err
		}
	}
	if raw, ok := object["retainedTail"]; ok {
		var messages []json.RawMessage
		if !isJSONArray(raw) || json.Unmarshal(raw, &messages) != nil {
			return nil, invalidField("compaction.retainedTail", "an array of messages")
		}
		compaction.RetainedTail = make([]piMessage, len(messages))
		for i, messageRaw := range messages {
			message, err := decodePiMessage(messageRaw, fmt.Sprintf("compaction.retainedTail[%d]", i))
			if err != nil {
				return nil, err
			}
			compaction.RetainedTail[i] = *message
		}
	}
	if firstKeptEntryID == nil {
		if _, ok := object["retainedTail"]; !ok {
			return nil, fmt.Errorf("%w: compaction requires firstKeptEntryId or retainedTail", ErrInvalidSession)
		}
	}
	return compaction, nil
}

func decodePiBranchSummary(object map[string]json.RawMessage) (*piBranchSummary, error) {
	fromID, err := requiredString(object, "fromId", "branch_summary.fromId")
	if err != nil {
		return nil, err
	}
	summary, err := requiredString(object, "summary", "branch_summary.summary")
	if err != nil {
		return nil, err
	}
	entry := &piBranchSummary{FromID: fromID, Summary: summary}
	if raw, ok := object["details"]; ok {
		entry.Details = cloneRaw(raw)
	}
	if raw, ok := object["fromHook"]; ok {
		value, err := decodeBool(raw, "branch_summary.fromHook")
		if err != nil {
			return nil, err
		}
		entry.FromHook = &value
	}
	if raw, ok := object["usage"]; ok {
		entry.Usage, err = decodePiUsage(raw, "branch_summary.usage")
		if err != nil {
			return nil, err
		}
	}
	return entry, nil
}

func decodePiCustom(object map[string]json.RawMessage) (*piCustom, error) {
	customType, err := requiredString(object, "customType", "custom.customType")
	if err != nil {
		return nil, err
	}
	entry := &piCustom{CustomType: customType}
	if raw, ok := object["data"]; ok {
		entry.Data = cloneRaw(raw)
	}
	return entry, nil
}

func decodePiCustomMessage(object map[string]json.RawMessage) (*piCustomMessage, error) {
	customType, err := requiredString(object, "customType", "custom_message.customType")
	if err != nil {
		return nil, err
	}
	contentRaw, ok := object["content"]
	if !ok {
		return nil, invalidField("custom_message.content", "present")
	}
	contentText, contentBlocks, err := decodeContent(contentRaw, "custom_message.content", true)
	if err != nil {
		return nil, err
	}
	display, err := requiredBool(object, "display", "custom_message.display")
	if err != nil {
		return nil, err
	}
	entry := &piCustomMessage{
		CustomType:    customType,
		Content:       cloneRaw(contentRaw),
		Display:       display,
		ContentText:   contentText,
		ContentBlocks: contentBlocks,
	}
	if raw, ok := object["details"]; ok {
		entry.Details = cloneRaw(raw)
	}
	return entry, nil
}

func decodePiLabel(object map[string]json.RawMessage) (*piLabel, error) {
	targetID, err := requiredString(object, "targetId", "label.targetId")
	if err != nil {
		return nil, err
	}
	label, err := optionalString(object, "label", "label.label", false)
	if err != nil {
		return nil, err
	}
	return &piLabel{TargetID: targetID, Label: label}, nil
}

func decodePiSessionInfo(object map[string]json.RawMessage) (*piSessionInfo, error) {
	name, err := optionalString(object, "name", "session_info.name", false)
	if err != nil {
		return nil, err
	}
	return &piSessionInfo{Name: name}, nil
}

func decodeContentField(object map[string]json.RawMessage, field, path string, allowString bool) (*string, []piContentBlock, error) {
	raw, ok := object[field]
	if !ok {
		return nil, nil, invalidField(path, "present")
	}
	return decodeContent(raw, path, allowString)
}

func decodeContent(raw []byte, path string, allowString bool) (*string, []piContentBlock, error) {
	if allowString && isJSONString(raw) {
		var text string
		if err := json.Unmarshal(raw, &text); err != nil {
			return nil, nil, invalidField(path, "a string or content array")
		}
		return &text, nil, nil
	}
	if !isJSONArray(raw) {
		if allowString {
			return nil, nil, invalidField(path, "a string or content array")
		}
		return nil, nil, invalidField(path, "a content array")
	}
	var rawBlocks []json.RawMessage
	if err := json.Unmarshal(raw, &rawBlocks); err != nil {
		return nil, nil, invalidField(path, "a content array")
	}
	blocks := make([]piContentBlock, len(rawBlocks))
	for i, rawBlock := range rawBlocks {
		blockPath := fmt.Sprintf("%s[%d]", path, i)
		object, err := decodeObject(rawBlock, blockPath)
		if err != nil {
			return nil, nil, err
		}
		blockType, err := requiredString(object, "type", blockPath+".type")
		if err != nil {
			return nil, nil, err
		}
		var block piContentBlock
		if err := json.Unmarshal(rawBlock, &block); err != nil {
			return nil, nil, fmt.Errorf("%w: %s contains a field with an invalid shape", ErrInvalidSession, blockPath)
		}
		block.Type = blockType
		block.Raw = cloneRaw(rawBlock)
		for _, field := range []string{"textSignature", "thinkingSignature", "thoughtSignature", "namespace"} {
			if _, err := optionalString(object, field, blockPath+"."+field, false); err != nil {
				return nil, nil, err
			}
		}
		if err := validateOptionalBool(object, "redacted", blockPath+".redacted"); err != nil {
			return nil, nil, err
		}
		switch blockType {
		case "text":
			if _, err := requiredString(object, "text", blockPath+".text"); err != nil {
				return nil, nil, err
			}
		case "image":
			for _, field := range []string{"data", "mimeType"} {
				if _, err := requiredString(object, field, blockPath+"."+field); err != nil {
					return nil, nil, err
				}
			}
		case "thinking":
			if _, err := requiredString(object, "thinking", blockPath+".thinking"); err != nil {
				return nil, nil, err
			}
		case "toolCall":
			for _, field := range []string{"id", "name"} {
				if _, err := requiredString(object, field, blockPath+"."+field); err != nil {
					return nil, nil, err
				}
			}
			arguments, ok := object["arguments"]
			if !ok {
				return nil, nil, invalidField(blockPath+".arguments", "an object")
			}
			if _, err := decodeObject(arguments, blockPath+".arguments"); err != nil {
				return nil, nil, err
			}
		}
		blocks[i] = block
	}
	return nil, blocks, nil
}

func validateDeferredHandle(raw []byte, path string) error {
	object, err := decodeObject(raw, path)
	if err != nil {
		return err
	}
	for _, field := range []string{"provider", "modelId", "api", "id"} {
		if _, err := requiredString(object, field, path+"."+field); err != nil {
			return err
		}
	}
	for _, field := range []string{"expiresAt", "pollAfterMs"} {
		if err := validateOptionalInt64(object, field, path+"."+field); err != nil {
			return err
		}
	}
	return nil
}

func validateDiagnostics(raw []byte, path string) error {
	if !isJSONArray(raw) {
		return invalidField(path, "an array")
	}
	var diagnostics []json.RawMessage
	if err := json.Unmarshal(raw, &diagnostics); err != nil {
		return invalidField(path, "an array")
	}
	for i, diagnosticRaw := range diagnostics {
		diagnosticPath := fmt.Sprintf("%s[%d]", path, i)
		object, err := decodeObject(diagnosticRaw, diagnosticPath)
		if err != nil {
			return err
		}
		if _, err := requiredString(object, "type", diagnosticPath+".type"); err != nil {
			return err
		}
		if _, err := requiredInt64(object, "timestamp", diagnosticPath+".timestamp"); err != nil {
			return err
		}
		if details, ok := object["details"]; ok {
			if _, err := decodeObject(details, diagnosticPath+".details"); err != nil {
				return err
			}
		}
		if errorRaw, ok := object["error"]; ok {
			errorObject, err := decodeObject(errorRaw, diagnosticPath+".error")
			if err != nil {
				return err
			}
			if _, err := requiredString(errorObject, "message", diagnosticPath+".error.message"); err != nil {
				return err
			}
			for _, field := range []string{"name", "stack"} {
				if _, err := optionalString(errorObject, field, diagnosticPath+".error."+field, false); err != nil {
					return err
				}
			}
			if code, ok := errorObject["code"]; ok && !isJSONString(code) {
				var number float64
				if isJSONNull(code) || json.Unmarshal(code, &number) != nil {
					return invalidField(diagnosticPath+".error.code", "a string or number")
				}
			}
		}
	}
	return nil
}

func validateOptionalBool(object map[string]json.RawMessage, field, path string) error {
	raw, ok := object[field]
	if !ok {
		return nil
	}
	_, err := decodeBool(raw, path)
	return err
}

func validateOptionalInt64(object map[string]json.RawMessage, field, path string) error {
	raw, ok := object[field]
	if !ok {
		return nil
	}
	var value int64
	if isJSONNull(raw) || json.Unmarshal(raw, &value) != nil {
		return invalidField(path, "an integer")
	}
	return nil
}

func encodePiRecord(record any) ([]byte, error) {
	var (
		encoded []byte
		err     error
	)
	switch value := record.(type) {
	case piHeader:
		encoded, err = encodePiHeader(value)
	case *piHeader:
		if value == nil {
			return nil, fmt.Errorf("%w: cannot encode a nil Pi header", ErrInvalidSession)
		}
		encoded, err = encodePiHeader(*value)
	case piEntry:
		encoded, err = encodePiEntry(value)
	case *piEntry:
		if value == nil {
			return nil, fmt.Errorf("%w: cannot encode a nil Pi entry", ErrInvalidSession)
		}
		encoded, err = encodePiEntry(*value)
	default:
		encoded, err = json.Marshal(record)
	}
	if err != nil {
		return nil, fmt.Errorf("encode Pi session record: %w", err)
	}
	if len(encoded) > maxSessionEntryBytes {
		return nil, sizeError(ErrSessionEntryTooLarge, maxSessionEntryBytes)
	}
	if _, err := decodeObject(encoded, "encoded session record"); err != nil {
		return nil, err
	}
	return encoded, nil
}

func encodePiHeader(header piHeader) ([]byte, error) {
	if len(header.Raw) > 0 {
		if len(header.Raw) > maxSessionEntryBytes {
			return nil, sizeError(ErrSessionEntryTooLarge, maxSessionEntryBytes)
		}
		if _, err := decodePiHeader(header.Raw); err != nil {
			return nil, err
		}
		return cloneRaw(header.Raw), nil
	}
	encoded, err := json.Marshal(header)
	if err != nil {
		return nil, err
	}
	if _, err := decodePiHeader(encoded); err != nil {
		return nil, err
	}
	return encoded, nil
}

func encodePiEntry(entry piEntry) ([]byte, error) {
	if len(entry.Raw) > 0 {
		if len(entry.Raw) > maxSessionEntryBytes {
			return nil, sizeError(ErrSessionEntryTooLarge, maxSessionEntryBytes)
		}
		if _, err := decodePiEntry(entry.Raw); err != nil {
			return nil, err
		}
		return cloneRaw(entry.Raw), nil
	}

	object := make(map[string]json.RawMessage, 5)
	for field, value := range map[string]any{
		"type":      entry.Type,
		"id":        entry.ID,
		"parentId":  entry.ParentID,
		"timestamp": entry.Timestamp,
	} {
		encoded, err := json.Marshal(value)
		if err != nil {
			return nil, err
		}
		object[field] = encoded
	}
	var payload any
	switch entry.Type {
	case "message":
		if entry.Message == nil {
			return nil, fmt.Errorf("%w: message payload is required", ErrInvalidSession)
		}
		encodedMessage, err := json.Marshal(entry.Message)
		if err != nil {
			return nil, err
		}
		object["message"] = encodedMessage
	case "model_change":
		payload = entry.ModelChange
	case "thinking_level_change":
		payload = entry.ThinkingLevelChange
	case "compaction":
		payload = entry.Compaction
	case "branch_summary":
		payload = entry.BranchSummary
	case "custom":
		payload = entry.Custom
	case "custom_message":
		payload = entry.CustomMessage
	case "label":
		payload = entry.Label
	case "session_info":
		payload = entry.SessionInfo
	default:
		return nil, fmt.Errorf("%w: raw JSON is required to encode an unknown entry type", ErrInvalidSession)
	}
	if entry.Type != "message" {
		if payload == nil || isNilJSONPayload(payload) {
			return nil, fmt.Errorf("%w: %s payload is required", ErrInvalidSession, entry.Type)
		}
		payloadJSON, err := json.Marshal(payload)
		if err != nil {
			return nil, err
		}
		payloadObject, err := decodeObject(payloadJSON, entry.Type+" payload")
		if err != nil {
			return nil, err
		}
		for key, value := range payloadObject {
			object[key] = value
		}
	}
	encoded, err := json.Marshal(object)
	if err != nil {
		return nil, err
	}
	if _, err := decodePiEntry(encoded); err != nil {
		return nil, err
	}
	return encoded, nil
}

func decodeObject(raw []byte, path string) (map[string]json.RawMessage, error) {
	var object map[string]json.RawMessage
	if err := json.Unmarshal(raw, &object); err != nil || object == nil {
		return nil, fmt.Errorf("%w: %s must be one JSON object", ErrInvalidSession, path)
	}
	return object, nil
}

func requiredObjectRaw(object map[string]json.RawMessage, field, path string) (json.RawMessage, error) {
	raw, ok := object[field]
	if !ok {
		return nil, invalidField(path, "an object")
	}
	if _, err := decodeObject(raw, path); err != nil {
		return nil, err
	}
	return raw, nil
}

func requiredString(object map[string]json.RawMessage, field, path string) (string, error) {
	raw, ok := object[field]
	if !ok || isJSONNull(raw) {
		return "", invalidField(path, "a string")
	}
	var value string
	if err := json.Unmarshal(raw, &value); err != nil {
		return "", invalidField(path, "a string")
	}
	return value, nil
}

func optionalString(object map[string]json.RawMessage, field, path string, allowNull bool) (*string, error) {
	raw, ok := object[field]
	if !ok {
		return nil, nil
	}
	if isJSONNull(raw) {
		if allowNull {
			return nil, nil
		}
		return nil, invalidField(path, "a string")
	}
	var value string
	if err := json.Unmarshal(raw, &value); err != nil {
		return nil, invalidField(path, "a string")
	}
	return &value, nil
}

func requiredNullableString(object map[string]json.RawMessage, field, path string) (*string, error) {
	raw, ok := object[field]
	if !ok {
		return nil, invalidField(path, "a string or null")
	}
	if isJSONNull(raw) {
		return nil, nil
	}
	var value string
	if err := json.Unmarshal(raw, &value); err != nil {
		return nil, invalidField(path, "a string or null")
	}
	return &value, nil
}

func requiredInt64(object map[string]json.RawMessage, field, path string) (int64, error) {
	raw, ok := object[field]
	if !ok || isJSONNull(raw) {
		return 0, invalidField(path, "an integer")
	}
	var value int64
	if err := json.Unmarshal(raw, &value); err != nil {
		return 0, invalidField(path, "an integer")
	}
	return value, nil
}

func requiredFloat64(object map[string]json.RawMessage, field, path string) (float64, error) {
	raw, ok := object[field]
	if !ok || isJSONNull(raw) {
		return 0, invalidField(path, "a number")
	}
	var value float64
	if err := json.Unmarshal(raw, &value); err != nil {
		return 0, invalidField(path, "a number")
	}
	return value, nil
}

func requiredBool(object map[string]json.RawMessage, field, path string) (bool, error) {
	raw, ok := object[field]
	if !ok || isJSONNull(raw) {
		return false, invalidField(path, "a boolean")
	}
	return decodeBool(raw, path)
}

func decodeBool(raw []byte, path string) (bool, error) {
	if isJSONNull(raw) {
		return false, invalidField(path, "a boolean")
	}
	var value bool
	if err := json.Unmarshal(raw, &value); err != nil {
		return false, invalidField(path, "a boolean")
	}
	return value, nil
}

func invalidField(path, shape string) error {
	return fmt.Errorf("%w: %s must be %s", ErrInvalidSession, path, shape)
}

func sizeError(kind error, limit int) error {
	return fmt.Errorf("%w: maximum is %d bytes", kind, limit)
}

func isJSONNull(raw []byte) bool {
	return bytes.Equal(bytes.TrimSpace(raw), []byte("null"))
}

func isJSONString(raw []byte) bool {
	trimmed := bytes.TrimSpace(raw)
	return len(trimmed) > 0 && trimmed[0] == '"'
}

func isJSONArray(raw []byte) bool {
	trimmed := bytes.TrimSpace(raw)
	return len(trimmed) > 0 && trimmed[0] == '['
}

func cloneRaw(raw []byte) json.RawMessage {
	return append(json.RawMessage(nil), raw...)
}

func isNilJSONPayload(value any) bool {
	encoded, err := json.Marshal(value)
	return err == nil && bytes.Equal(encoded, []byte("null"))
}
