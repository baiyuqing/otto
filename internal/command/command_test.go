package command

import "testing"

func TestCommandsNonEmptyNoDuplicates(t *testing.T) {
	if len(Commands) == 0 {
		t.Fatal("Commands is empty")
	}
	seen := make(map[string]bool, len(Commands))
	for _, c := range Commands {
		if seen[c.Name] {
			t.Fatalf("duplicate command name: %s", c.Name)
		}
		seen[c.Name] = true
	}
}

func TestMatch(t *testing.T) {
	tests := []struct {
		name  string
		value string
		want  []string
	}{
		{name: "empty", value: "", want: nil},
		{name: "no leading slash", value: "help", want: nil},
		{name: "exact", value: "/help", want: []string{"/help"}},
		{name: "prefix", value: "/re", want: []string{"/resume", "/remember"}},
		{name: "no match", value: "/nope", want: nil},
		{name: "rejects newline", value: "/help\n", want: nil},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			matches := Match(tt.value)
			if len(matches) != len(tt.want) {
				t.Fatalf("Match(%q) = %v, want %v", tt.value, names(matches), tt.want)
			}
			for i, m := range matches {
				if m.Name != tt.want[i] {
					t.Fatalf("Match(%q) = %v, want %v", tt.value, names(matches), tt.want)
				}
			}
		})
	}
}

func TestParse(t *testing.T) {
	c, argument, ok := Parse("  /compact focus on tests  ")
	if !ok || c.Kind != KindCompact || argument != "focus on tests" {
		t.Fatalf("Parse() = %+v, %q, %v", c, argument, ok)
	}

	c, argument, ok = Parse("/help")
	if !ok || c.Kind != KindHelp || argument != "" {
		t.Fatalf("Parse() = %+v, %q, %v", c, argument, ok)
	}

	_, _, ok = Parse("/nope")
	if ok {
		t.Fatal("Parse(\"/nope\") reported ok, want false")
	}
}

func TestFind(t *testing.T) {
	if _, ok := Find("/exit"); !ok {
		t.Fatal("Find(\"/exit\") not found")
	}
	if _, ok := Find("/nope"); ok {
		t.Fatal("Find(\"/nope\") unexpectedly found")
	}
}

func names(commands []Command) []string {
	result := make([]string, len(commands))
	for i, c := range commands {
		result[i] = c.Name
	}
	return result
}
