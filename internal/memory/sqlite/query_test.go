package sqlite

import (
	"errors"
	"strings"
	"testing"
	"time"

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

func TestBuildFTSLiteralExpressionExactBounds(t *testing.T) {
	t.Parallel()

	queryAtLimit := strings.Repeat(" ", memory.MaxQueryBytes-len("boundary")) + "boundary"
	termsAtLimit := strings.TrimSpace(strings.Repeat("a ", memory.MaxFTSTerms))
	accepted := []struct {
		name, input string
		terms       int
	}{
		{"query bytes", queryAtLimit, 1},
		{"term bytes", strings.Repeat("q", memory.MaxFTSTermBytes), 1},
		{"term count", termsAtLimit, memory.MaxFTSTerms},
	}
	for _, test := range accepted {
		expression, err := buildFTSLiteralExpression(test.input)
		if err != nil {
			t.Fatalf("%s rejected: %v", test.name, err)
		}
		if got := strings.Count(expression, `"`) / 2; got != test.terms {
			t.Fatalf("%s terms = %d, want %d", test.name, got, test.terms)
		}
	}

	rejected := []struct {
		name, input string
	}{
		{"query bytes", queryAtLimit + "x"},
		{"term bytes", strings.Repeat("q", memory.MaxFTSTermBytes+1)},
		{"term count", termsAtLimit + " a"},
		{"invalid UTF-8", string([]byte{'o', 'k', 0xff})},
	}
	for _, test := range rejected {
		_, err := buildFTSLiteralExpression(test.input)
		if !errors.Is(err, memory.ErrInvalidRequest) {
			t.Fatalf("%s error = %v", test.name, err)
		}
		if strings.Contains(err.Error(), test.input) {
			t.Fatalf("%s error echoed query", test.name)
		}
	}
}

func TestRetrieverSeedQueriesKeepApprovedCapsAndDeferLabels(t *testing.T) {
	t.Parallel()
	request := memory.RetrievalRequest{
		Scopes: []memory.Scope{{Namespace: memory.NamespaceUser, ID: "query-user"}, {Namespace: memory.NamespaceWorkspace, ID: "query-workspace"}},
		Labels: []string{"must-be-deferred"}, Now: time.Date(2026, 8, 29, 20, 0, 0, 0, time.UTC),
	}
	baseline, baselineArgs := buildBaselineQuery(request, request.Scopes[0])
	lexical, lexicalArgs := buildFTSCandidateQuery(request, `"literal"`)
	replacement, replacementArgs := buildWorkspaceReplacementQuery(request, request.Scopes[1], []retrievalKey{{kind: "preference", key: "editor"}})
	for name, seed := range map[string]struct {
		query string
		args  []any
	}{
		"baseline": {baseline, baselineArgs}, "lexical": {lexical, lexicalArgs}, "replacement": {replacement, replacementArgs},
	} {
		if strings.Contains(seed.query, "json_each") || strings.Contains(seed.query, "must-be-deferred") {
			t.Fatalf("%s applied requested labels in SQL: %s", name, seed.query)
		}
		for _, argument := range seed.args {
			if argument == "must-be-deferred" {
				t.Fatalf("%s bound requested label in seed SQL", name)
			}
		}
	}
	if got := baselineArgs[len(baselineArgs)-1]; got != memory.MaxBaselineRecords {
		t.Fatalf("baseline cap = %v", got)
	}
	if got := lexicalArgs[len(lexicalArgs)-1]; got != memory.MaxRetrievalCandidates {
		t.Fatalf("lexical cap = %v", got)
	}
	if got := replacementArgs[len(replacementArgs)-1]; got != memory.MaxRetrievalCandidates {
		t.Fatalf("replacement cap = %v", got)
	}
}

func TestBuildFTSLiteralExpressionQuoteLiterals(t *testing.T) {
	t.Parallel()
	for input, want := range map[string]string{
		`a"b`:      `"a""b"`,
		`"quoted"`: `"""quoted"""`,
		`"`:        "",
	} {
		got, err := buildFTSLiteralExpression(input)
		if err != nil || got != want {
			t.Fatalf("build %q = %q, %v; want %q", input, got, err, want)
		}
	}
}
