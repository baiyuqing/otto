package session

import (
	"encoding/json"
	"fmt"

	"github.com/baiyuqing/otto/internal/model"
)

type piOttoDetails struct {
	TaskID       string `json:"taskId,omitempty"`
	UsagePresent bool   `json:"usagePresent,omitempty"`
}

type piDetails struct {
	Otto *piOttoDetails `json:"otto,omitempty"`
}

func encodePiOttoDetails(details piOttoDetails) (json.RawMessage, error) {
	raw, err := json.Marshal(piDetails{Otto: &details})
	if err != nil {
		return nil, fmt.Errorf("encode Otto Pi details: %w", err)
	}
	return raw, nil
}

func decodePiOttoDetails(raw json.RawMessage) (*piOttoDetails, error) {
	if len(raw) == 0 || string(raw) == "null" {
		return nil, nil
	}
	var details piDetails
	if err := json.Unmarshal(raw, &details); err != nil {
		return nil, nil
	}
	if details.Otto == nil {
		return nil, nil
	}
	if details.Otto.TaskID != "" {
		if err := (model.ContextMetadata{TaskID: details.Otto.TaskID}).Validate(); err != nil {
			return nil, fmt.Errorf("%w: invalid context metadata: %v", ErrInvalidSession, err)
		}
	}
	return details.Otto, nil
}
