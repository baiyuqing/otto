package config

import (
	"strings"
	"testing"
)

func TestResolveModelLimitsIncludesEveryBaselineAlias(t *testing.T) {
	tests := []struct {
		name    string
		models  []string
		context int
		hard    int
		working int
		output  int
	}{
		{
			name:    "GPT 5.6",
			models:  []string{"gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"},
			context: 1_050_000, hard: 922_000, working: 272_000, output: 128_000,
		},
		{
			name:    "GPT 5.5",
			models:  []string{"gpt-5.5", "gpt-5.5-pro"},
			context: 1_050_000, hard: 922_000, working: 272_000, output: 128_000,
		},
		{
			name:    "GPT 5.4",
			models:  []string{"gpt-5.4", "gpt-5.4-pro"},
			context: 1_050_000, hard: 922_000, working: 272_000, output: 128_000,
		},
		{
			name:    "GPT 5.4 small",
			models:  []string{"gpt-5.4-mini", "gpt-5.4-nano"},
			context: 400_000, hard: 272_000, working: 272_000, output: 128_000,
		},
		{
			name:    "GPT 5.3",
			models:  []string{"gpt-5.3-codex"},
			context: 400_000, hard: 272_000, working: 272_000, output: 128_000,
		},
		{
			name:    "GPT 5.3 Spark",
			models:  []string{"gpt-5.3-codex-spark"},
			context: 128_000, hard: 128_000, working: 128_000, output: 32_000,
		},
		{
			name:    "GPT 5.2",
			models:  []string{"gpt-5.2", "gpt-5.2-pro", "gpt-5.2-codex"},
			context: 400_000, hard: 272_000, working: 272_000, output: 128_000,
		},
		{
			name: "GPT 5.1",
			models: []string{
				"gpt-5.1", "gpt-5.1-codex", "gpt-5.1-codex-mini", "gpt-5.1-codex-max",
			},
			context: 400_000, hard: 272_000, working: 272_000, output: 128_000,
		},
		{
			name:    "GPT 5",
			models:  []string{"gpt-5", "gpt-5-pro", "gpt-5-mini", "gpt-5-nano", "gpt-5-codex"},
			context: 400_000, hard: 272_000, working: 272_000, output: 128_000,
		},
		{
			name: "GPT chat latest",
			models: []string{
				"gpt-5.3-chat-latest", "gpt-5.2-chat-latest", "gpt-5.1-chat-latest", "gpt-5-chat-latest",
			},
			context: 128_000, hard: 128_000, working: 128_000, output: 16_384,
		},
		{
			name:    "GPT 4.1",
			models:  []string{"gpt-4.1", "gpt-4.1-mini", "gpt-4.1-nano"},
			context: 1_047_576, hard: 1_047_576, working: 1_047_576, output: 32_768,
		},
		{
			name:    "GPT 4o",
			models:  []string{"gpt-4o", "gpt-4o-mini"},
			context: 128_000, hard: 128_000, working: 128_000, output: 16_384,
		},
		{
			name:    "o series",
			models:  []string{"o1", "o1-pro", "o3", "o3-mini", "o3-pro", "o4-mini"},
			context: 200_000, hard: 200_000, working: 200_000, output: 100_000,
		},
		{
			name:    "Claude 5",
			models:  []string{"claude-fable-5", "claude-opus-5", "claude-sonnet-5"},
			context: 1_000_000, hard: 1_000_000, working: 1_000_000, output: 128_000,
		},
		{
			name: "Claude 4.6 plus",
			models: []string{
				"claude-opus-4-8", "claude-opus-4.8", "claude-opus-4-7", "claude-opus-4.7",
				"claude-opus-4-6", "claude-opus-4.6", "claude-sonnet-4-6", "claude-sonnet-4.6",
			},
			context: 1_000_000, hard: 1_000_000, working: 1_000_000, output: 128_000,
		},
		{
			name:    "Claude Sonnet 4.5",
			models:  []string{"claude-sonnet-4-5", "claude-sonnet-4.5"},
			context: 1_000_000, hard: 1_000_000, working: 1_000_000, output: 64_000,
		},
		{
			name:    "Claude 4.5 200K",
			models:  []string{"claude-opus-4-5", "claude-opus-4.5", "claude-haiku-4-5", "claude-haiku-4.5"},
			context: 200_000, hard: 200_000, working: 200_000, output: 64_000,
		},
		{
			name:    "Claude Opus 4",
			models:  []string{"claude-opus-4-1", "claude-opus-4.1", "claude-opus-4"},
			context: 200_000, hard: 200_000, working: 200_000, output: 32_000,
		},
		{
			name:    "Claude Sonnet 4 and 3.7",
			models:  []string{"claude-sonnet-4", "claude-3-7-sonnet", "claude-3.7-sonnet"},
			context: 200_000, hard: 200_000, working: 200_000, output: 64_000,
		},
		{
			name:    "Claude 3.5",
			models:  []string{"claude-3-5-sonnet", "claude-3.5-sonnet", "claude-3-5-haiku", "claude-3.5-haiku"},
			context: 200_000, hard: 200_000, working: 200_000, output: 8_192,
		},
		{
			name:    "Claude Haiku 3",
			models:  []string{"claude-3-haiku"},
			context: 200_000, hard: 200_000, working: 200_000, output: 4_096,
		},
	}

	seen := make(map[string]bool)
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			for _, model := range test.models {
				if seen[model] {
					t.Fatalf("duplicate test model %q", model)
				}
				seen[model] = true
				got := resolveModelLimits(model)
				if !got.Known || got.ContextWindow != test.context || got.HardInputWindow != test.hard ||
					got.WorkingWindow != test.working || got.MaxOutputTokens != test.output {
					t.Errorf("resolveModelLimits(%q) = %#v", model, got)
				}
				if !strings.HasPrefix(got.SourceURL, "https://") {
					t.Errorf("resolveModelLimits(%q).SourceURL = %q, want https:// URL", model, got.SourceURL)
				}
			}
		})
	}
}

func TestResolveModelLimitsUsesExactAliasesAndWrappers(t *testing.T) {
	tests := []struct {
		model   string
		known   bool
		context int
		hard    int
		working int
		output  int
	}{
		{"gpt-5.6-sol", true, 1_050_000, 922_000, 272_000, 128_000},
		{"openai/gpt-5.6-sol:batch", true, 1_050_000, 922_000, 272_000, 128_000},
		{"gpt-5.3-codex-spark", true, 128_000, 128_000, 128_000, 32_000},
		{"gpt-4o-2024-05-13", true, 128_000, 128_000, 128_000, 4_096},
		{"gpt-4o-2024-08-06", true, 128_000, 128_000, 128_000, 16_384},
		{"gpt-4o-2024-11-20", true, 128_000, 128_000, 128_000, 16_384},
		{"gpt-4o-mini-2024-07-18", true, 128_000, 128_000, 128_000, 16_384},
		{"anthropic/claude-sonnet-4.5", true, 1_000_000, 1_000_000, 1_000_000, 64_000},
		{"claude-sonnet-4-5-20250929", true, 1_000_000, 1_000_000, 1_000_000, 64_000},
		{"gpt-5.6-sol-2026-01-02", true, 1_050_000, 922_000, 272_000, 128_000},
		{"openai/gpt-5.6-sol-2026-01-02:batch", true, 1_050_000, 922_000, 272_000, 128_000},
		{"claude-opus-4.8-20261001:batch", true, 1_000_000, 1_000_000, 1_000_000, 128_000},
		{"OPENAI/gpt-5.6-sol", false, 0, 0, 0, 0},
		{"azure-gpt-5.6-sol", false, 0, 0, 0, 0},
		{"my-gpt-model", false, 0, 0, 0, 0},
		{"anthropic/gpt-5.6-sol", false, 0, 0, 0, 0},
		{"openai/claude-sonnet-4-5", false, 0, 0, 0, 0},
		{"openai/openai/gpt-5.6-sol", false, 0, 0, 0, 0},
		{"gpt-5.6-sol:batch:batch", false, 0, 0, 0, 0},
		{"gpt-5.6-sol:batch-extra", false, 0, 0, 0, 0},
		{"gpt-5.6-sol-2026-01-02-extra", false, 0, 0, 0, 0},
		{"gpt-5.6-sol-preview-2026-01-02", false, 0, 0, 0, 0},
		{"claude-sonnet-4-5-20250929-extra", false, 0, 0, 0, 0},
		{"claude-sonnet-4-5-preview-20250929", false, 0, 0, 0, 0},
	}
	for _, test := range tests {
		got := resolveModelLimits(test.model)
		if got.Known != test.known || got.ContextWindow != test.context ||
			got.HardInputWindow != test.hard || got.WorkingWindow != test.working ||
			got.MaxOutputTokens != test.output {
			t.Errorf("resolveModelLimits(%q) = %#v", test.model, got)
		}
		if !test.known && got.SourceURL != "" {
			t.Errorf("resolveModelLimits(%q).SourceURL = %q, want empty", test.model, got.SourceURL)
		}
	}
}
