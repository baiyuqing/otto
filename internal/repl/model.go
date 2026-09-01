package repl

import (
	"context"
	"fmt"
	"strings"

	"github.com/baiyuqing/otto/internal/app"
)

// modelCommand handles "/model" and "/model <profile>". Bare shows the current
// profile/provider/model and lists configured profiles; a named profile starts
// a fresh session on it, reusing the /new session-replacement machinery.
func (r *REPL) modelCommand(ctx context.Context, args string) (bool, error) {
	switcher, ok := r.backend.(app.ProfileSwitcher)
	if !ok {
		return false, &commandError{command: "/model", err: app.ErrProfileSwitchUnavailable}
	}
	if args == "" {
		info := r.backend.Info()
		_, _ = fmt.Fprintf(r.stdout, "Current: profile %s (provider %s, model %s)\n", info.Profile, info.Provider, info.Model)
		if profiles := switcher.Profiles(); len(profiles) > 0 {
			_, _ = fmt.Fprintf(r.stdout, "Profiles: %s\n", strings.Join(profiles, ", "))
		} else {
			_, _ = fmt.Fprintln(r.stdout, "No profiles configured.")
		}
		return false, nil
	}
	if _, err := switcher.SwitchProfile(ctx, args); err != nil {
		return false, &commandError{command: "/model", err: err}
	}
	if err := switcher.SetDefaultProfile(ctx, args); err != nil {
		return false, &commandError{command: "/model", err: err}
	}
	info := r.backend.Info()
	_, _ = fmt.Fprintf(r.stdout, "Switched to profile %s (provider %s, model %s). Set as default profile.\n", info.Profile, info.Provider, info.Model)
	if info.SessionID != "" {
		_, _ = fmt.Fprintf(r.stdout, "Session: %s\n", info.SessionID)
	}
	return false, nil
}
