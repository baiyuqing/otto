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
