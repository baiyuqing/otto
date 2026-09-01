// Package urlprivacy conservatively extracts URL userinfo forms that must be
// treated as private at process and provider boundaries.
package urlprivacy

import (
	"net/url"
	"strings"

	"github.com/baiyuqing/otto/internal/safetext"
)

// UserinfoForms returns raw and independently decoded userinfo components.
// malformed reports that the nonempty value is not a normal absolute URL with
// a parseable authority, contains invalid escape/control syntax, or uses a
// backslash-ambiguous shape.
func UserinfoForms(raw string) (values []string, malformed bool) {
	if raw == "" {
		return nil, false
	}

	authority, normalAuthority := normalURLAuthority(raw)
	percentValid := validPercentEscapes(raw)
	if normalAuthority {
		separator := strings.Index(raw, "://")
		authorityURL, authorityErr := url.Parse(raw[:separator+3] + authority)
		authorityParsed := authorityErr == nil && authorityURL.Scheme != "" && authorityURL.Host != ""
		if authorityParsed {
			if userinfoEnd := strings.LastIndexByte(authority, '@'); userinfoEnd >= 0 {
				values = appendUserinfoForms(values, authority[:userinfoEnd])
				values = appendParsedUserinfoForms(values, authorityURL.User)
			}
			_, parseErr := url.Parse(raw)
			return deduplicate(values), parseErr != nil || !percentValid || strings.ContainsRune(raw, '\\')
		}
		// Preserve the lexically delimited authority userinfo even when parsing
		// that authority fails and a later path segment also contains '@'.
		if userinfoEnd := strings.LastIndexByte(authority, '@'); userinfoEnd >= 0 {
			values = appendUserinfoForms(values, authority[:userinfoEnd])
		}
	}

	// Only scan outside the normally delimited authority when that authority
	// cannot be established. This remains conservative for malformed proxy
	// spellings without reclassifying a valid URL's path/query/fragment '@'.
	for _, candidate := range lexicalUserinfoCandidates(raw) {
		values = appendUserinfoForms(values, candidate)
	}
	return deduplicate(values), true
}

func normalURLAuthority(raw string) (string, bool) {
	separator := strings.Index(raw, "://")
	if separator <= 0 || !validScheme(raw[:separator]) {
		return "", false
	}
	start := separator + 3
	if start >= len(raw) || raw[start] == '/' || raw[start] == '\\' {
		return "", false
	}
	end := len(raw)
	if relativeEnd := strings.IndexAny(raw[start:], "/?#\\"); relativeEnd >= 0 {
		end = start + relativeEnd
	}
	if end == start {
		return "", false
	}
	return raw[start:end], true
}

func lexicalUserinfoCandidates(raw string) []string {
	end := len(raw)
	if queryOrFragment := strings.IndexAny(raw, "?#"); queryOrFragment >= 0 {
		end = queryOrFragment
	}
	if end == 0 {
		return nil
	}

	start := 0
	if separator := strings.Index(raw[:end], "://"); separator >= 0 {
		start = separator + 3
	} else if strings.HasPrefix(raw[:end], "//") {
		start = 2
	} else if colon := strings.IndexByte(raw[:end], ':'); colon >= 0 && colon+1 < end && (raw[colon+1] == '/' || raw[colon+1] == '\\') {
		start = colon + 1
	}
	for start < end && (raw[start] == '/' || raw[start] == '\\') {
		start++
	}
	if start >= end {
		return nil
	}

	prefix := raw[start:end]
	at := strings.LastIndexByte(prefix, '@')
	if at < 0 {
		return nil
	}
	beforeAt := prefix[:at]
	candidateStart := 0
	if separator := strings.LastIndexAny(beforeAt, "/\\"); separator >= 0 {
		candidateStart = separator + 1
	}
	candidate := beforeAt[candidateStart:]
	if candidate == "" {
		return nil
	}
	candidates := []string{candidate}

	if colon := strings.IndexByte(candidate, ':'); colon > 0 && validScheme(candidate[:colon]) {
		alternative := strings.TrimLeft(candidate[colon+1:], "/\\")
		if alternative != "" {
			candidates = append(candidates, alternative)
		}
	}
	return candidates
}

func appendUserinfoForms(values []string, rawUserinfo string) []string {
	values = append(values, rawUserinfo)
	rawUsername, rawPassword, hasPassword := strings.Cut(rawUserinfo, ":")
	values = append(values, rawUsername)
	if decoded, err := url.PathUnescape(rawUsername); err == nil {
		values = append(values, decoded)
	}
	if hasPassword {
		values = append(values, rawPassword)
		if decoded, err := url.PathUnescape(rawPassword); err == nil {
			values = append(values, decoded)
		}
	}
	if decoded, err := url.PathUnescape(rawUserinfo); err == nil {
		values = append(values, decoded)
	}
	return values
}

func appendParsedUserinfoForms(values []string, user *url.Userinfo) []string {
	if user == nil {
		return values
	}
	username := user.Username()
	values = append(values, username)
	decodedUserinfo := username
	if password, ok := user.Password(); ok {
		values = append(values, password)
		decodedUserinfo += ":" + password
	}
	return append(values, decodedUserinfo, user.String())
}

func deduplicate(values []string) []string {
	seen := make(map[string]struct{}, len(values))
	result := make([]string, 0, len(values))
	for _, value := range values {
		value = safetext.CanonicalizeUTF8(value)
		if value == "" {
			continue
		}
		if _, duplicate := seen[value]; duplicate {
			continue
		}
		seen[value] = struct{}{}
		result = append(result, strings.Clone(value))
	}
	return result
}

func validScheme(scheme string) bool {
	if scheme == "" || !isASCIILetter(scheme[0]) {
		return false
	}
	for index := 1; index < len(scheme); index++ {
		character := scheme[index]
		if isASCIILetter(character) || character >= '0' && character <= '9' || character == '+' || character == '-' || character == '.' {
			continue
		}
		return false
	}
	return true
}

func isASCIILetter(character byte) bool {
	return character >= 'A' && character <= 'Z' || character >= 'a' && character <= 'z'
}

func validPercentEscapes(raw string) bool {
	for index := 0; index < len(raw); index++ {
		if raw[index] != '%' {
			continue
		}
		if index+2 >= len(raw) || !isHex(raw[index+1]) || !isHex(raw[index+2]) {
			return false
		}
		index += 2
	}
	return true
}

func isHex(character byte) bool {
	return character >= '0' && character <= '9' ||
		character >= 'A' && character <= 'F' ||
		character >= 'a' && character <= 'f'
}
