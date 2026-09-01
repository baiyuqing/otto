package tui

import (
	"fmt"
	"math"
	"strconv"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/app"
	"github.com/baiyuqing/otto/internal/model"
)

func TestResponsiveHelpOverlayDescribesDetailToggle(t *testing.T) {
	content := helpOverlayContent(100, 20, app.SandboxInfo{
		Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true, Reason: app.SandboxReasonNone,
	})
	if !strings.Contains(content, "toggle details") {
		t.Fatalf("help overlay = %q, want detail toggle wording", content)
	}
	if strings.Contains(content, "toggle tool arguments/output") {
		t.Fatalf("help overlay = %q, want old tool-only wording removed", content)
	}
}

func TestFormatFooterTokenCountBoundaries(t *testing.T) {
	tests := []struct {
		name   string
		tokens int
		want   string
	}{
		{name: "negative clamps to zero", tokens: -1, want: "0"},
		{name: "zero stays zero", tokens: 0, want: "0"},
		{name: "sub-thousand stays raw", tokens: 999, want: "999"},
		{name: "one thousand becomes k", tokens: 1000, want: "1k"},
		{name: "rounds down below half tenth", tokens: 1049, want: "1k"},
		{name: "rounds up at half tenth", tokens: 1050, want: "1.1k"},
		{name: "rounds to one and three tenths", tokens: 1250, want: "1.3k"},
		{name: "keeps largest sub-k boundary under a million", tokens: 999949, want: "999.9k"},
		{name: "promotes rounded million boundary", tokens: 999950, want: "1M"},
		{name: "one million becomes M", tokens: 1000000, want: "1M"},
		{name: "millions keep one decimal", tokens: 1250000, want: "1.3M"},
		{name: "promotes rounded billion boundary", tokens: 999950000, want: "1B"},
		{name: "billions keep one decimal", tokens: 1250000000, want: "1.3B"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := formatFooterTokenCount(tt.tokens); got != tt.want {
				t.Fatalf("formatFooterTokenCount(%d) = %q, want %q", tt.tokens, got, tt.want)
			}
		})
	}
}

func TestFormatFooterTokenCountMaxIntBoundaries(t *testing.T) {
	want := "9223372036.9B"
	if strconv.IntSize == 32 {
		want = "2.1B"
	}
	if got := formatFooterTokenCount(math.MaxInt); got != want {
		t.Fatalf("formatFooterTokenCount(math.MaxInt) = %q, want %q", got, want)
	}
}

func TestRenderFooterUsesHumanReadableTokenTotals(t *testing.T) {
	info := app.Info{Profile: "profile", Model: "model", SessionID: "session"}
	tests := []struct {
		name        string
		usage       model.Usage
		want        string
		wantMissing []string
	}{
		{name: "compacts totals and cached counts", usage: model.Usage{InputTokens: 1250, OutputTokens: 56800, CachedInputTokens: 45700}, want: "tokens 1.3k/56.8k (cached 45.7k)"},
		{name: "keeps zero usage explicit", usage: model.Usage{}, want: "tokens 0/0", wantMissing: []string{"cached"}},
		{name: "omits nonpositive cached totals", usage: model.Usage{InputTokens: 1250, OutputTokens: 56800, CachedInputTokens: -1}, want: "tokens 1.3k/56.8k", wantMissing: []string{"cached"}},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			footer := renderFooter(120, info, tt.usage, "")
			if !strings.Contains(footer, tt.want) {
				t.Fatalf("footer = %q, want %q", footer, tt.want)
			}
			for _, missing := range tt.wantMissing {
				if strings.Contains(footer, missing) {
					t.Fatalf("footer = %q, want missing %q", footer, missing)
				}
			}
		})
	}
}

func TestRenderFooterShowsContextPercentageStates(t *testing.T) {
	base := app.Info{Profile: "profile", Model: "model", SessionID: "session", ContextWindow: 128_000}
	tests := []struct {
		name        string
		info        app.Info
		want        string
		wantMissing []string
	}{
		{name: "known latest context", info: app.Info{Profile: base.Profile, Model: base.Model, SessionID: base.SessionID, ContextWindow: base.ContextWindow, ContextInputTokens: 29_952, ContextInputTokensPresent: true}, want: "23.4%"},
		{name: "pending after compaction", info: app.Info{Profile: base.Profile, Model: base.Model, SessionID: base.SessionID, ContextWindow: base.ContextWindow, ContextInputTokensPending: true}, want: "ctx ?%"},
		{name: "unknown window hides field", info: app.Info{Profile: base.Profile, Model: base.Model, SessionID: base.SessionID, ContextInputTokens: 29_952, ContextInputTokensPresent: true}, wantMissing: []string{"ctx "}},
		{name: "omitted prompt usage hides field", info: base, wantMissing: []string{"ctx "}},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			footer := renderFooter(120, tt.info, model.Usage{InputTokens: 20, OutputTokens: 6}, "")
			if tt.want != "" && !strings.Contains(footer, tt.want) {
				t.Fatalf("footer = %q, want %q", footer, tt.want)
			}
			for _, missing := range tt.wantMissing {
				if strings.Contains(footer, missing) {
					t.Fatalf("footer = %q, want missing %q", footer, missing)
				}
			}
		})
	}
}

func TestFormatFooterContextPercentage(t *testing.T) {
	tests := []struct {
		name   string
		input  int
		window int
		want   string
	}{
		{name: "zero input", input: 0, window: 128_000, want: "0.0%"},
		{name: "rounds to one decimal", input: 1, window: 6, want: "16.7%"},
		{name: "exact percentage", input: 29_952, window: 128_000, want: "23.4%"},
		{name: "over one hundred is not clamped", input: 1_100, window: 1_000, want: "110.0%"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := formatFooterContextPercentage(tt.input, tt.window); got != tt.want {
				t.Fatalf("formatFooterContextPercentage(%d, %d) = %q, want %q", tt.input, tt.window, got, tt.want)
			}
		})
	}
}

func TestFooterContextFieldRendersProgressBar(t *testing.T) {
	tests := []struct {
		name    string
		info    app.Info
		wantBar bool   // expect bar characters (▌ or ░)
		wantPct string // percentage text that must appear
	}{
		{
			name:    "low usage shows bar and percentage",
			info:    app.Info{Profile: "p", Model: "m", ContextWindow: 128_000, ContextInputTokens: 29_952, ContextInputTokensPresent: true},
			wantBar: true,
			wantPct: "23.4%",
		},
		{
			name:    "high usage shows bar and percentage",
			info:    app.Info{Profile: "p", Model: "m", ContextWindow: 100_000, ContextInputTokens: 90_000, ContextInputTokensPresent: true},
			wantBar: true,
			wantPct: "90.0%",
		},
		{
			name:    "pending shows no bar",
			info:    app.Info{Profile: "p", Model: "m", ContextWindow: 128_000, ContextInputTokensPending: true},
			wantBar: false,
			wantPct: "?%",
		},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			field := footerContextField(tt.info)
			if !strings.Contains(field, tt.wantPct) {
				t.Fatalf("field = %q, want percentage %q", field, tt.wantPct)
			}
			hasBar := strings.ContainsRune(field, '▌') || strings.ContainsRune(field, '░')
			if tt.wantBar && !hasBar {
				t.Fatalf("field = %q, want progress bar characters", field)
			}
			if !tt.wantBar && hasBar {
				t.Fatalf("field = %q, want no bar characters in pending state", field)
			}
		})
	}
}

func TestRenderFooterContextFieldStaysWithinBounds(t *testing.T) {
	info := app.Info{Profile: "profile", Model: "model", SessionID: "session", ContextWindow: 128_000, ContextInputTokens: 29_952, ContextInputTokensPresent: true}
	for _, width := range []int{48, 60, 72, 120} {
		t.Run(fmt.Sprintf("width-%d", width), func(t *testing.T) {
			footer := renderFooter(width, info, model.Usage{InputTokens: 20, OutputTokens: 6}, "status")
			assertRenderedBounds(t, footer, width, 1)
		})
	}
}

func TestRenderFooterKeepsZeroUsageRenderableUnderPTYFixtures(t *testing.T) {
	info := app.Info{Profile: "profile", Model: "model", SessionID: "session"}
	footer := renderFooter(120, info, model.Usage{}, "")
	if !strings.Contains(footer, "tokens 0/0") {
		t.Fatalf("footer = %q, want zero-usage tokens marker", footer)
	}
}

func TestRenderFooterHandlesMaxIntUsageWithoutOverflow(t *testing.T) {
	info := app.Info{Profile: "profile", Model: "model", SessionID: "session"}
	footer := renderFooter(120, info, model.Usage{InputTokens: int(^uint(0) >> 1), OutputTokens: int(^uint(0) >> 1)}, "")
	if !strings.Contains(footer, "tokens ") || !strings.Contains(footer, "B") {
		t.Fatalf("footer = %q, want large-token B suffix without panic", footer)
	}
}

func TestRenderFooterKeepsSandboxBadgeAcrossSupportedWidths(t *testing.T) {
	states := []struct {
		name  string
		info  app.SandboxInfo
		badge string
	}{
		{
			name:  "seatbelt",
			info:  app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true, Reason: app.SandboxReasonNone},
			badge: "sb",
		},
		{
			name:  "off",
			info:  app.SandboxInfo{Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true, Reason: app.SandboxReasonNone},
			badge: "unsafe",
		},
		{
			name:  "unavailable",
			info:  app.SandboxInfo{Mode: app.SandboxUnavailable, Network: app.SandboxNetworkDenied, BashAvailable: false, Reason: app.SandboxReasonSelfTestFailed},
			badge: "no-bash",
		},
	}

	for _, state := range states {
		for _, width := range []int{minTerminalWidth, 48, 60, 72, 120} {
			t.Run(fmt.Sprintf("%s-width-%d", state.name, width), func(t *testing.T) {
				info := app.Info{
					Workspace: "/workspace/" + strings.Repeat("workspace", 20),
					Profile:   "profile", Model: "model", SessionID: strings.Repeat("session", 20),
					ContextWindow: 128_000, ContextInputTokens: 100_000, ContextInputTokensPresent: true,
					Sandbox: state.info,
				}
				footer := renderFooter(width, info, model.Usage{InputTokens: 123_456, OutputTokens: 78_901, CachedInputTokens: 100_000}, "working")
				assertRenderedBounds(t, footer, width, 1)
				fields := strings.Split(strings.TrimSpace(footer), " | ")
				found := false
				for _, field := range fields {
					if field == state.badge {
						found = true
					}
				}
				if !found {
					t.Fatalf("footer = %q, want fixed Sandbox badge %q", footer, state.badge)
				}
			})
		}
	}
}

func TestSandboxOverlaysRetainCompletePolicyAtSupportedSizes(t *testing.T) {
	states := []struct {
		name string
		info app.SandboxInfo
	}{
		{
			name: "seatbelt allowed",
			info: app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true, Reason: app.SandboxReasonNone},
		},
		{
			name: "seatbelt denied",
			info: app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkDenied, BashAvailable: true, Reason: app.SandboxReasonNone},
		},
		{
			name: "off",
			info: app.SandboxInfo{Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: true, Reason: app.SandboxReasonNone},
		},
		{
			name: "unavailable",
			info: app.SandboxInfo{Mode: app.SandboxUnavailable, Network: app.SandboxNetworkDenied, BashAvailable: false, Reason: app.SandboxReasonSelfTestFailed},
		},
	}
	sizes := []struct{ width, height int }{{40, 8}, {40, 20}, {60, 12}, {100, 20}}

	for _, state := range states {
		for _, size := range sizes {
			t.Run(fmt.Sprintf("%s-%dx%d", state.name, size.width, size.height), func(t *testing.T) {
				backend := &fakeBackend{info: app.Info{
					SessionID: "session", SessionPath: "/tmp/session.jsonl", Provider: "openai-compatible", Profile: "profile", Model: "model",
					Sandbox: state.info,
				}}
				model := resizeModel(t, newTestModelWithBackend(t, backend), size.width, size.height)
				for _, overlay := range []overlayKind{overlayHelp, overlaySession} {
					model.overlay = overlay
					rendered := model.View().Content
					assertRenderedBounds(t, rendered, size.width, size.height)
					for _, word := range strings.Fields("Sandbox: " + state.info.Summary()) {
						if !strings.Contains(rendered, word) {
							t.Fatalf("overlay %d at %dx%d omitted Sandbox policy word %q: %q", overlay, size.width, size.height, word, rendered)
						}
					}
				}
			})
		}
	}
}

func TestSandboxSessionOverlayReasonRequiresExplicitUnavailableMode(t *testing.T) {
	payload := "invalid\x1b]52;c;owned\a\nforged"
	tests := []struct {
		name       string
		info       app.SandboxInfo
		wantReason string
		forbidden  []string
	}{
		{
			name: "valid available",
			info: app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkAllowed, BashAvailable: true, Reason: app.SandboxReasonNone},
		},
		{
			name:      "malformed seatbelt with approved reason",
			info:      app.SandboxInfo{Mode: app.SandboxSeatbelt, Network: app.SandboxNetworkUnconfined, BashAvailable: true, Reason: app.SandboxReasonSelfTestFailed},
			forbidden: []string{"self-test-failed"},
		},
		{
			name:      "malformed off with control reason",
			info:      app.SandboxInfo{Mode: app.SandboxOff, Network: app.SandboxNetworkUnconfined, BashAvailable: false, Reason: app.SandboxReason(payload)},
			forbidden: []string{payload},
		},
		{
			name:      "unknown mode with approved reason",
			info:      app.SandboxInfo{Mode: app.SandboxMode("future-mode"), Network: app.SandboxNetworkDenied, Reason: app.SandboxReasonSeatbeltMissing},
			forbidden: []string{"future-mode", "seatbelt-missing"},
		},
		{
			name:      "control mode and reason",
			info:      app.SandboxInfo{Mode: app.SandboxMode(payload), Network: app.SandboxNetwork(payload), Reason: app.SandboxReason(payload)},
			forbidden: []string{payload},
		},
		{
			name:       "unavailable valid",
			info:       app.SandboxInfo{Mode: app.SandboxUnavailable, Network: app.SandboxNetworkDenied, Reason: app.SandboxReasonSelfTestFailed},
			wantReason: "self-test-failed",
		},
		{
			name:       "unavailable empty reason",
			info:       app.SandboxInfo{Mode: app.SandboxUnavailable, Network: app.SandboxNetworkDenied, Reason: app.SandboxReasonNone},
			wantReason: "runtime-failure",
		},
		{
			name:       "unavailable invalid reason",
			info:       app.SandboxInfo{Mode: app.SandboxUnavailable, Network: app.SandboxNetworkDenied, Reason: app.SandboxReason(payload)},
			wantReason: "runtime-failure",
			forbidden:  []string{payload},
		},
		{
			name:       "unavailable with Bash",
			info:       app.SandboxInfo{Mode: app.SandboxUnavailable, Network: app.SandboxNetworkDenied, BashAvailable: true, Reason: app.SandboxReasonSelfTestFailed},
			wantReason: "runtime-failure",
			forbidden:  []string{"self-test-failed"},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			model := resizeModel(t, newTestModelWithBackend(t, &fakeBackend{info: app.Info{SessionID: "session", Sandbox: tt.info}}), 100, 20)
			model.overlay = overlayHelp
			help := model.View().Content
			if strings.Contains(help, "Sandbox reason:") || tt.wantReason != "" && strings.Contains(help, tt.wantReason) {
				t.Fatalf("help overlay exposed unavailable reason: %q", help)
			}

			model.overlay = overlaySession
			session := model.View().Content
			assertRenderedBounds(t, session, 100, 20)
			if tt.wantReason == "" {
				if strings.Contains(session, "Sandbox reason:") {
					t.Fatalf("session overlay rendered a reason outside explicit unavailable mode: %q", session)
				}
			} else if !strings.Contains(session, "Sandbox reason: "+tt.wantReason) {
				t.Fatalf("session overlay missing reason %q: %q", tt.wantReason, session)
			}
			for _, forbidden := range tt.forbidden {
				if strings.Contains(session, forbidden) {
					t.Fatalf("session overlay leaked raw or inconsistent state %q: %q", forbidden, session)
				}
			}
			if strings.ContainsAny(session, "\x1b\a\r\t") {
				t.Fatalf("session overlay contains raw Sandbox controls: %q", session)
			}
		})
	}
}

func TestEmptyTranscriptHintStaysWithinBounds(t *testing.T) {
	for _, width := range []int{minTerminalWidth, 60, 100} {
		t.Run(fmt.Sprintf("width-%d", width), func(t *testing.T) {
			hint := emptyTranscriptHint(width)
			if hint == "" {
				t.Fatalf("hint = %q, want non-empty", hint)
			}
			assertRenderedBounds(t, hint, width, 5)
		})
	}
}
