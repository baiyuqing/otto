package urlprivacy

import (
	"slices"
	"testing"
	"unicode/utf8"
)

func TestUserinfoFormsDistinguishesAuthorityFromPathQueryAndFragment(t *testing.T) {
	tests := []struct {
		name      string
		raw       string
		want      []string
		malformed bool
	}{
		{
			name: "balanced IPv6 authority userinfo",
			raw:  "https://raw%20user:raw%2Fpass@[2001:db8::1]:8443/path?next=@ignored#@ignored",
			want: []string{"raw%20user:raw%2Fpass", "raw%20user", "raw%2Fpass", "raw user:raw/pass", "raw user", "raw/pass"},
		},
		{
			name: "valid path query and fragment at signs",
			raw:  "https://[2001:db8::1]:8443/path/user:pass@example.test?next=user:pass@example.test#user:pass@example.test",
		},
		{
			name:      "malformed query after valid authority does not reclassify path at",
			raw:       "https://[2001:db8::1]:8443/path/user:pass@example.test?broken=%zz",
			malformed: true,
		},
		{
			name:      "malformed normal authority is retained before later path at",
			raw:       "https://bad%zz:pass%2Fword@[::1]:8443/path/user:other@example.test",
			want:      []string{"bad%zz:pass%2Fword", "bad%zz", "pass%2Fword", "pass/word"},
			malformed: true,
		},
		{
			name:      "host parse failure scans later authority-like at",
			raw:       "https://[::1/path/raw%20user:raw%2Fpass@example.test",
			want:      []string{"raw%20user:raw%2Fpass", "raw%20user", "raw%2Fpass", "raw user", "raw/pass"},
			malformed: true,
		},
		{
			name:      "ambiguous extra slash",
			raw:       "https:///raw%20user:raw%2Fpass@example.test/path",
			want:      []string{"raw%20user:raw%2Fpass", "raw%20user", "raw%2Fpass", "raw user:raw/pass", "raw user", "raw/pass"},
			malformed: true,
		},
		{
			name:      "backslash authority",
			raw:       `https:\\raw%20user:raw%2Fpass@example.test\path`,
			want:      []string{"raw%20user:raw%2Fpass", "raw%20user", "raw%2Fpass", "raw user:raw/pass", "raw user", "raw/pass"},
			malformed: true,
		},
		{
			name:      "missing scheme",
			raw:       "raw%20user:raw%2Fpass@example.test/path",
			want:      []string{"raw%20user:raw%2Fpass", "raw%20user", "raw%2Fpass", "raw user:raw/pass", "raw user", "raw/pass"},
			malformed: true,
		},
		{
			name:      "independently decodable password",
			raw:       "https://bad%zz:pass%2Fword@[::1]:8443/path",
			want:      []string{"bad%zz:pass%2Fword", "bad%zz", "pass%2Fword", "pass/word"},
			malformed: true,
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			got, malformed := UserinfoForms(test.raw)
			if malformed != test.malformed {
				t.Fatalf("UserinfoForms() malformed = %t, want %t; values=%#v", malformed, test.malformed, got)
			}
			for _, want := range test.want {
				if !slices.Contains(got, want) {
					t.Fatalf("UserinfoForms() omitted %q: %#v", want, got)
				}
			}
			if len(test.want) == 0 && len(got) != 0 {
				t.Fatalf("UserinfoForms() misclassified non-authority userinfo: %#v", got)
			}
			for _, value := range got {
				if !utf8.ValidString(value) {
					t.Fatalf("UserinfoForms() returned invalid UTF-8: %q", value)
				}
			}
		})
	}
}

func TestUserinfoFormsReportsMalformedProxyShapesWithoutUserinfo(t *testing.T) {
	for _, raw := range []string{
		"https://example.test/path%zz",
		"https:///example.test/path",
		`https:\\example.test\path`,
		"example.test:8443",
	} {
		t.Run(raw, func(t *testing.T) {
			values, malformed := UserinfoForms(raw)
			if !malformed || len(values) != 0 {
				t.Fatalf("UserinfoForms(%q) = %#v, malformed %t; want no forms and malformed", raw, values, malformed)
			}
		})
	}
}

func TestUserinfoFormsCanonicalizesEveryIndependentlyDecodedComponent(t *testing.T) {
	values, malformed := UserinfoForms("https://user%FF:pass%C0%AF@[::1]:8443")
	if malformed {
		t.Fatal("valid percent escapes were classified as malformed")
	}
	for _, want := range []string{"user%FF:pass%C0%AF", "user%FF", "pass%C0%AF", "user�:pass��", "user�", "pass��"} {
		if !slices.Contains(values, want) {
			t.Fatalf("UserinfoForms() omitted canonical/raw form %q: %#v", want, values)
		}
	}
}
