package memory

import (
	"crypto/rand"
	"encoding/hex"
	"fmt"
)

const randomIDBytes = 16

func NewID() (string, error) {
	var raw [randomIDBytes]byte
	if _, err := rand.Read(raw[:]); err != nil {
		return "", fmt.Errorf("%w: generate memory ID", ErrUnavailable)
	}
	return hex.EncodeToString(raw[:]), nil
}

// GenerateDistinctIDs calls generate until it has count distinct IDs.
func GenerateDistinctIDs(count int, generate func() (string, error)) ([]string, error) {
	if count < 0 || generate == nil {
		return nil, fmt.Errorf("%w: distinct memory ID generator", ErrInvalidRequest)
	}
	ids := make([]string, 0, count)
	seen := make(map[string]struct{}, count)
	for len(ids) < count {
		duplicateRetries := 0
		for {
			id, err := generate()
			if err != nil {
				return nil, fmt.Errorf("%w: generate distinct memory ID", ErrUnavailable)
			}
			if _, duplicate := seen[id]; duplicate {
				if duplicateRetries == MaxDuplicateIDRetries {
					return nil, fmt.Errorf("%w: generate distinct memory ID", ErrUnavailable)
				}
				duplicateRetries++
				continue
			}
			seen[id] = struct{}{}
			ids = append(ids, id)
			break
		}
	}
	return ids, nil
}
