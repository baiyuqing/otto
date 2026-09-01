// Package urlprivacy conservatively extracts URL userinfo forms that must be
// treated as private at process and provider boundaries.
package urlprivacy

import (
	"net/url"
	"strings"

	"github.com/baiyuqing/otto/internal/safetext"
)

// UserinfoForms returns raw and independently decoded userinfo components.
// ambiguous reports that userinfo extraction cannot be proven complete. Values
// without a literal '@' are complete for this credential detector even when
// they are not semantically usable proxy URLs.
func UserinfoForms(raw string) (values []string, ambiguous bool) {
	if raw == "" || !strings.ContainsRune(raw, '@') {
		return nil, false
	}

	authority, normalAuthority := normalURLAuthority(raw)
	if normalAuthority {
		atCount := strings.Count(authority, "@")
		switch atCount {
		case 0:
			if strings.ContainsRune(raw, '\\') {
				for _, candidate := range lexicalUserinfoCandidates(raw) {
					values = appendUserinfoForms(values, candidate)
				}
				return deduplicate(values), true
			}
			if !parseableAuthority(authority) {
				for _, candidate := range lexicalUserinfoCandidates(raw) {
					values = appendUserinfoForms(values, candidate)
				}
				return deduplicate(values), true
			}
			_, parseErr := url.Parse(raw)
			return nil, parseErr != nil || !validPercentEscapes(raw) || strings.ContainsRune(raw, '\\')
		case 1:
			userinfoEnd := strings.IndexByte(authority, '@')
			values = appendUserinfoForms(values, authority[:userinfoEnd])
			authorityURL, authorityErr := url.Parse("//" + authority)
			if authorityErr == nil && authorityURL.Host != "" {
				values = appendParsedUserinfoForms(values, authorityURL.User)
			}
			_, parseErr := url.Parse(raw)
			ambiguous = authorityErr != nil || authorityURL == nil || authorityURL.Host == "" ||
				parseErr != nil || !validPercentEscapes(raw) || strings.ContainsRune(raw, '\\')
			if strings.ContainsRune(raw, '\\') {
				for _, candidate := range lexicalUserinfoCandidates(raw) {
					values = appendUserinfoForms(values, candidate)
				}
			}
			return deduplicate(values), ambiguous
		default:
			for _, candidate := range userinfoCandidates(authority) {
				values = appendUserinfoForms(values, candidate)
			}
			if strings.ContainsRune(raw, '\\') {
				for _, candidate := range lexicalUserinfoCandidates(raw) {
					values = appendUserinfoForms(values, candidate)
				}
			}
			return deduplicate(values), true
		}
	}

	// A malformed authority delimiter makes every segment immediately before an
	// '@' plausible. Retain both local and cumulative interpretations so one
	// malformed/multi-'@' spelling cannot hide an earlier credential.
	for _, candidate := range lexicalUserinfoCandidates(raw) {
		values = appendUserinfoForms(values, candidate)
	}
	return deduplicate(values), true
}

func normalURLAuthority(raw string) (string, bool) {
	start := 0
	switch {
	case strings.HasPrefix(raw, "//"):
		start = 2
	default:
		separator := strings.Index(raw, "://")
		if separator <= 0 || !validScheme(raw[:separator]) {
			return "", false
		}
		start = separator + 3
	}
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

func parseableAuthority(authority string) bool {
	parsed, err := url.Parse("//" + authority)
	return err == nil && parsed != nil && parsed.Host != ""
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
	return userinfoCandidates(raw[start:end])
}

func userinfoCandidates(raw string) []string {
	var candidates []string
	for offset := 0; offset < len(raw); {
		relativeAt := strings.IndexByte(raw[offset:], '@')
		if relativeAt < 0 {
			break
		}
		at := offset + relativeAt
		beforeAt := raw[:at]
		localStart := strings.LastIndexAny(beforeAt, "/\\@") + 1
		candidates = appendUserinfoCandidate(candidates, beforeAt[localStart:])
		if localStart > 0 {
			candidates = appendUserinfoCandidate(candidates, beforeAt)
		}
		offset = at + 1
	}
	return candidates
}

func appendUserinfoCandidate(candidates []string, candidate string) []string {
	if candidate == "" {
		return candidates
	}
	candidates = append(candidates, candidate)
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
