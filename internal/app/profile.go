package app

import "context"

// ProfileSelectionResult records whether the session replacement completed.
// Switched remains true when saving the default profile fails afterward.
type ProfileSelectionResult struct {
	ResumeResult
	Switched bool
}

func SelectProfile(ctx context.Context, switcher ProfileSwitcher, profile string) (ProfileSelectionResult, error) {
	result, err := switcher.SwitchProfile(ctx, profile)
	if err != nil {
		return ProfileSelectionResult{}, err
	}
	selection := ProfileSelectionResult{ResumeResult: result, Switched: true}
	if err := switcher.SetDefaultProfile(ctx, profile); err != nil {
		return selection, err
	}
	return selection, nil
}
