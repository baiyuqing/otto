package config

import (
	"bytes"
	"errors"
	"reflect"

	"github.com/pelletier/go-toml/v2"
	"github.com/pelletier/go-toml/v2/unstable"
)

// UpdateSandbox replaces only the sandbox table, preserving other TOML bytes.
// Unusual layouts are rejected rather than rewritten destructively.
func UpdateSandbox(content []byte, settings SandboxConfig) ([]byte, error) {
	if _, err := ResolveSandbox(settings, nil); err != nil {
		return nil, err
	}
	before := map[string]any{}
	if err := toml.Unmarshal(content, &before); err != nil {
		return nil, errors.New("invalid configuration")
	}
	block, err := toml.Marshal(struct {
		Sandbox SandboxConfig `toml:"sandbox"`
	}{settings})
	if err != nil {
		return nil, err
	}
	start, end := -1, len(content)
	var parser unstable.Parser
	parser.Reset(content)
	for parser.NextExpression() {
		node := parser.Expression()
		if node.Kind != unstable.Table && node.Kind != unstable.ArrayTable {
			continue
		}
		keys := node.Key()
		keys.Next()
		key := keys.Node()
		// Locate the header line using the parsed key, never text inside a value.
		offset := int(key.Raw.Offset)
		line := bytes.LastIndexByte(content[:offset], '\n') + 1
		if start >= 0 {
			end = line
			break
		}
		if string(key.Data) == "sandbox" && !keys.Next() && node.Kind == unstable.Table {
			start = line
		}
	}
	if parser.Error() != nil {
		return nil, errors.New("invalid configuration")
	}
	var updated []byte
	if start < 0 {
		updated = append(append(bytes.Clone(content), '\n'), block...)
	} else {
		updated = append(updated, content[:start]...)
		updated = append(updated, block...)
		updated = append(updated, content[end:]...)
	}
	var after, want map[string]any
	if toml.Unmarshal(updated, &after) != nil || toml.Unmarshal(block, &want) != nil {
		return nil, errors.New("setup requires a separate [sandbox] table; configuration was not changed")
	}
	if !reflect.DeepEqual(after["sandbox"], want["sandbox"]) {
		return nil, errors.New("unsupported sandbox layout")
	}
	delete(before, "sandbox")
	delete(after, "sandbox")
	if !reflect.DeepEqual(before, after) {
		return nil, errors.New("setup would change unrelated configuration")
	}
	return updated, nil
}
