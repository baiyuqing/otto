// Package urlprivacy conservatively extracts URL userinfo forms that must be
// treated as private at process and provider boundaries.
package urlprivacy

import (
	"net/url"
	"strings"

	"github.com/baiyuqing/otto/internal/safetext"
)

const maxUserinfoInputBytes = 16 << 10

// UserinfoForms returns raw and independently decoded userinfo components.
// ambiguous reports that userinfo extraction cannot be proven complete. Values
// without a literal '@' are complete for this credential detector even when
// they are not semantically usable proxy URLs.
func UserinfoForms(raw string) (values []string, ambiguous bool) {
	if raw == "" || !strings.ContainsRune(raw, '@') {
		return nil, false
	}
	if len(raw) > maxUserinfoInputBytes {
		return nil, true
	}

	collector := newUserinfoCollector()
	addCandidates := func(raw string) bool {
		return collector.addCandidates(raw)
	}

	authority, normalAuthority := normalURLAuthority(raw)
	if normalAuthority {
		atCount := strings.Count(authority, "@")
		switch atCount {
		case 0:
			if strings.ContainsRune(raw, '\\') || !parseableAuthority(authority) {
				if !addCandidates(lexicalUserinfoCandidates(raw)) {
					return collector.values, true
				}
				return collector.values, true
			}
			_, parseErr := url.Parse(raw)
			return nil, parseErr != nil || !validPercentEscapes(raw) || strings.ContainsRune(raw, '\\')
		case 1:
			userinfoEnd := strings.IndexByte(authority, '@')
			if !collector.addForms(authority[:userinfoEnd]) {
				return collector.values, true
			}
			authorityURL, authorityErr := url.Parse("//" + authority)
			if authorityErr == nil && authorityURL.Host != "" && !collector.addParsedUserinfoForms(authorityURL.User) {
				return collector.values, true
			}
			_, parseErr := url.Parse(raw)
			ambiguous = authorityErr != nil || authorityURL == nil || authorityURL.Host == "" ||
				parseErr != nil || !validPercentEscapes(raw) || strings.ContainsRune(raw, '\\')
			if strings.ContainsRune(raw, '\\') && !addCandidates(lexicalUserinfoCandidates(raw)) {
				return collector.values, true
			}
			return collector.values, ambiguous
		default:
			if !addCandidates(authority) {
				return collector.values, true
			}
			if strings.ContainsRune(raw, '\\') && !addCandidates(lexicalUserinfoCandidates(raw)) {
				return collector.values, true
			}
			return collector.values, true
		}
	}

	if !addCandidates(lexicalUserinfoCandidates(raw)) {
		return collector.values, true
	}
	return collector.values, true
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

func lexicalUserinfoCandidates(raw string) string {
	end := len(raw)
	if queryOrFragment := strings.IndexAny(raw, "?#"); queryOrFragment >= 0 {
		end = queryOrFragment
	}
	if end == 0 {
		return ""
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
		return ""
	}
	return raw[start:end]
}

func userinfoCandidateSegments(raw string) ([]string, string) {
	var (
		candidates    []string
		finalBeforeAt string
	)
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
			finalBeforeAt = beforeAt
		}
		offset = at + 1
	}
	return candidates, finalBeforeAt
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

type userinfoCollector struct {
	values []string
	seen   map[string]struct{}
	bytes  int
}

func newUserinfoCollector() *userinfoCollector {
	return &userinfoCollector{seen: make(map[string]struct{})}
}

func (c *userinfoCollector) add(value string) bool {
	value = safetext.CanonicalizeUTF8(value)
	if value == "" {
		return true
	}
	if _, duplicate := c.seen[value]; duplicate {
		return true
	}
	if len(c.values) >= safetext.MaxSecretValues || c.bytes > safetext.MaxSecretBytes-len(value) {
		return false
	}
	c.seen[value] = struct{}{}
	c.values = append(c.values, value)
	c.bytes += len(value)
	return true
}

func (c *userinfoCollector) addCandidates(raw string) bool {
	locals, finalBeforeAt := userinfoCandidateSegments(raw)
	for _, candidate := range locals {
		if !c.addForms(candidate) {
			return false
		}
	}
	if finalBeforeAt != "" && !c.add(finalBeforeAt) {
		return false
	}
	return true
}

func (c *userinfoCollector) addForms(rawUserinfo string) bool {
	if !c.add(rawUserinfo) {
		return false
	}
	rawUsername, rawPassword, hasPassword := strings.Cut(rawUserinfo, ":")
	if !c.add(rawUsername) {
		return false
	}
	if decoded, err := url.PathUnescape(rawUsername); err == nil && !c.add(decoded) {
		return false
	}
	if hasPassword {
		if !c.add(rawPassword) {
			return false
		}
		if decoded, err := url.PathUnescape(rawPassword); err == nil && !c.add(decoded) {
			return false
		}
	}
	if decoded, err := url.PathUnescape(rawUserinfo); err == nil && !c.add(decoded) {
		return false
	}
	return true
}

func (c *userinfoCollector) addParsedUserinfoForms(user *url.Userinfo) bool {
	if user == nil {
		return true
	}
	username := user.Username()
	if !c.add(username) {
		return false
	}
	decodedUserinfo := username
	if password, ok := user.Password(); ok {
		if !c.add(password) {
			return false
		}
		decodedUserinfo += ":" + password
	}
	return c.add(decodedUserinfo) && c.add(user.String())
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
