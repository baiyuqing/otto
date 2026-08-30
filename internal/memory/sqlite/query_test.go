package sqlite

import (
	"errors"
	"strings"
	"testing"

	"github.com/baiyuqing/otto/internal/memory"
)

func TestBuildFTSLiteralExpression(t *testing.T) {
	t.Parallel()
	for _, test := range []struct {
		name, input, want string
	}{
		{"empty", "", ""},
		{"whitespace", " \t\n", ""},
		{"punctuation", `* : -- "`, ""},
		{"terms", "alpha beta", `"alpha" OR "beta"`},
		{"syntax", `alpha OR key:value*`, `"alpha" OR "OR" OR "key:value*"`},
		{"quotes", `alpha"beta`, `"alpha""beta"`},
		{"unicode", "你好 café", `"你好" OR "café"`},
	} {
		t.Run(test.name, func(t *testing.T) {
			got, err := buildFTSLiteralExpression(test.input)
			if err != nil || got != test.want {
				t.Fatalf("build = %q, %v; want %q", got, err, test.want)
			}
		})
	}
}

func TestBuildFTSLiteralExpressionBounds(t *testing.T) {
	t.Parallel()
	inputs := []string{
		strings.Repeat("q", memory.MaxQueryBytes+1),
		strings.Repeat("a ", memory.MaxFTSTerms) + "a",
		strings.Repeat("é", memory.MaxFTSTermBytes/2+1),
	}
	for _, input := range inputs {
		_, err := buildFTSLiteralExpression(input)
		if !errors.Is(err, memory.ErrInvalidRequest) {
			t.Fatalf("error = %v", err)
		}
		if strings.Contains(err.Error(), input) {
			t.Fatal("error echoed query")
		}
	}
}
