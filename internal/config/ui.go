package config

import (
	"fmt"
	"strings"
)

type UIMode string

const (
	UIAuto UIMode = "auto"
	UITUI  UIMode = "tui"
	UIRepl UIMode = "repl"
)

type UI struct {
	Mode string `toml:"mode"`
}

func ResolveUIMode(file File, env map[string]string, override string) (UIMode, error) {
	for _, candidate := range []string{override, envValue(env, "OTTO_UI"), file.UI.Mode} {
		if mode, ok, err := parseUIMode(candidate); err != nil {
			return "", err
		} else if ok {
			return mode, nil
		}
	}
	return UIAuto, nil
}

func parseUIMode(value string) (UIMode, bool, error) {
	value = strings.TrimSpace(value)
	if value == "" {
		return "", false, nil
	}

	switch UIMode(strings.ToLower(value)) {
	case UIAuto:
		return UIAuto, true, nil
	case UITUI:
		return UITUI, true, nil
	case UIRepl:
		return UIRepl, true, nil
	default:
		return "", false, fmt.Errorf("invalid ui mode %q: must be one of auto, tui, repl", value)
	}
}
