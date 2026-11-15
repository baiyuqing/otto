package bridge

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"log/slog"
	"strings"
	"time"

	"github.com/baiyuqing/otto/chat-connector/internal/manage"
)

const manageTimeout = 30 * time.Second

// newRequestID returns the id of one admitted inbound message: 32 lowercase
// hex characters from 16 random bytes.
func newRequestID() string {
	var raw [16]byte
	if _, err := rand.Read(raw[:]); err != nil {
		return "unavailable"
	}
	return hex.EncodeToString(raw[:])
}

// cmdManage handles all reserved configuration-management commands. It never
// forwards text to the ACP agent. Returns true when text was reserved.
func (c *chat) cmdManage(m Message, text string) bool {
	if text == "/models" || strings.HasPrefix(text, "/models ") {
		c.models(m, strings.TrimSpace(strings.TrimPrefix(text, "/models")))
		return true
	}
	if text == "/config" || strings.HasPrefix(text, "/config ") {
		c.config(m, strings.Fields(strings.TrimSpace(strings.TrimPrefix(text, "/config"))))
		return true
	}
	return false
}

func (c *chat) manager(m Message) (*manage.Client, bool) {
	if c.b.opts.Manage != nil {
		return c.b.opts.Manage, true
	}
	// The session-level switch needs no management client, so point at it
	// instead of leaving the sender without a way forward.
	c.notify(m.MessageID, "This command requires otto acp --attach connected to a running otto serve. Use /model <provider>/<model_id> to switch this conversation's model.")
	return nil, false
}
func (c *chat) manageContext() (context.Context, context.CancelFunc) {
	return context.WithTimeout(c.b.workCtx, manageTimeout)
}
func manageError(err error) string {
	if api, ok := err.(*manage.APIError); ok {
		switch api.Code {
		case "models_unsupported":
			return "The chatgpt provider cannot list models; use a model ID your account can use."
		case "provider_not_found":
			return "Provider not found. Send /config models."
		case "model_in_use":
			return "That model is the default model or the approval reviewer model. Select another model first."
		case "change_not_found":
			return "That configuration confirmation is unknown or was already used. Submit the change again."
		case "change_expired":
			return "That configuration preview expired. Submit the change again."
		case "change_stale":
			return "Configuration changed since the preview. Submit the change again."
		}
	}
	return "Management request failed: " + err.Error()
}

// validProviderName reports whether name is a provider name: ASCII letters,
// digits, '_' and '-'.
func validProviderName(name string) bool {
	if name == "" {
		return false
	}
	for _, r := range name {
		if !isSelectorWord(r) {
			return false
		}
	}
	return true
}

const (
	configUsage     = "Usage: /config [models|show <provider>|use|add|set|remove <provider>/<model_id>|confirm|cancel]"
	badSelectorText = "Expected a model selector <provider>/<model_id>."
)

func (c *chat) models(m Message, name string) {
	go func() {
		client, ok := c.manager(m)
		if !ok {
			return
		}
		if !validProviderName(name) {
			c.notify(m.MessageID, "Usage: /models <provider>. Send /config models to see the configured models.")
			return
		}
		ctx, cancel := c.manageContext()
		defer cancel()
		ids, err := client.Models(ctx, name)
		if err != nil {
			c.notify(m.MessageID, manageError(err))
			return
		}
		if len(ids) == 0 {
			c.notify(m.MessageID, "No models were returned for provider "+name+".")
			return
		}
		lines := make([]string, 0, len(ids))
		for _, id := range ids {
			lines = append(lines, name+"/"+id)
		}
		c.notify(m.MessageID, "Models of "+name+":\n"+strings.Join(lines, "\n"))
	}()
}

// renderConfiguredModels lists every model selector, marking the default.
func renderConfiguredModels(all manage.Providers) string {
	var lines []string
	for _, provider := range all.Providers {
		for _, model := range provider.Models {
			mark := ""
			if model.Default || model.Selector == all.DefaultModel {
				mark = " (default)"
			}
			lines = append(lines, "• "+model.Selector+mark)
		}
	}
	if len(lines) == 0 {
		lines = []string{"No models are configured."}
	}
	return "Configured models:\n" + strings.Join(lines, "\n") +
		"\nUse /config show <provider>, /models <provider>, /config use <provider>/<model_id>, or /config add <provider>/<model_id>."
}

func renderProvider(p manage.Provider) string {
	lines := []string{p.Name, "type: " + p.Type}
	if p.BaseURL != "" {
		lines = append(lines, "base_url: "+p.BaseURL)
	}
	if p.APIKeyEnv != "" {
		lines = append(lines, "api_key_env: "+p.APIKeyEnv)
	}
	if len(p.Models) == 0 {
		lines = append(lines, "models: none")
	}
	for _, model := range p.Models {
		line := "• " + model.Selector
		if model.Thinking != "" {
			line += " (thinking " + model.Thinking + ")"
		}
		lines = append(lines, line)
	}
	return strings.Join(lines, "\n")
}

func (c *chat) config(m Message, args []string) {
	go func() {
		client, ok := c.manager(m)
		if !ok {
			return
		}
		ctx, cancel := c.manageContext()
		defer cancel()
		if len(args) == 0 {
			args = []string{"models"}
		}
		switch args[0] {
		case "models":
			all, err := client.Providers(ctx)
			if err != nil {
				c.notify(m.MessageID, manageError(err))
				return
			}
			c.notify(m.MessageID, renderConfiguredModels(all))
		case "show":
			if len(args) != 2 || !validProviderName(args[1]) {
				c.notify(m.MessageID, "Usage: /config show <provider>")
				return
			}
			p, err := client.Provider(ctx, args[1])
			if err != nil {
				c.notify(m.MessageID, manageError(err))
				return
			}
			c.notify(m.MessageID, renderProvider(p))
		case "confirm":
			c.confirmChange(m, client, args)
		case "cancel":
			c.cancelChange(m, client, args)
		case "use", "remove":
			if len(args) != 2 {
				c.notify(m.MessageID, "Usage: /config "+args[0]+" <provider>/<model_id>")
				return
			}
			if !ValidModelSelector(args[1]) {
				c.notify(m.MessageID, badSelectorText)
				return
			}
			kind := "set_default_model"
			if args[0] == "remove" {
				kind = "remove_model"
			}
			c.previewChange(m, client, map[string]any{"kind": kind, "model": args[1]})
		case "add":
			if len(args) < 2 || len(args)%2 != 0 {
				c.notify(m.MessageID, "Usage: /config add <provider>/<model_id> [--thinking <level>]")
				return
			}
			if !ValidModelSelector(args[1]) {
				c.notify(m.MessageID, badSelectorText)
				return
			}
			change := map[string]any{"kind": "set_model", "model": args[1]}
			for i := 2; i+1 < len(args); i += 2 {
				switch args[i] {
				case "--base-url", "--api-key-env", "--provider":
					c.notify(m.MessageID, hostOnlyMessage)
					return
				case "--thinking":
					change["thinking"] = args[i+1]
				default:
					c.notify(m.MessageID, "Unknown /config add option: "+args[i])
					return
				}
			}
			c.previewChange(m, client, change)
		case "set":
			if len(args) != 4 {
				c.notify(m.MessageID, "Usage: /config set <provider>/<model_id> thinking <level>")
				return
			}
			if !ValidModelSelector(args[1]) {
				c.notify(m.MessageID, badSelectorText)
				return
			}
			field := strings.ReplaceAll(args[2], "-", "_")
			switch field {
			case "thinking":
			case "base_url", "api_key_env":
				c.notify(m.MessageID, hostOnlyMessage)
				return
			default:
				c.notify(m.MessageID, "Only thinking can be changed.")
				return
			}
			c.previewChange(m, client, map[string]any{"kind": "set_model_field", "model": args[1], "field": field, "value": args[3]})
		default:
			c.notify(m.MessageID, configUsage)
		}
	}()
}

// hostOnlyMessage answers chat requests to change base_url or api_key_env.
// These fields select which environment variable is sent to which URL, so
// only the host user may change them.
const hostOnlyMessage = "Base URL and API-key variable can only be changed on the host (web UI, TUI, or the otto config file)."

func (c *chat) previewChange(m Message, client *manage.Client, change any) {
	requestID := m.RequestID
	if requestID == "" {
		requestID = newRequestID()
	}
	ctx, cancel := c.manageContext()
	defer cancel()
	ctx = manage.WithRequestID(ctx, requestID)
	preview, err := client.Preview(ctx, change)
	if err != nil {
		slog.Warn("management_failed", "request_id", requestID, "platform", m.Platform, "chat_id", m.ChatID, "command", "config", "error", err)
		c.notify(m.MessageID, manageError(err)+" (request_id="+requestID+")")
		return
	}
	c.manageMu.Lock()
	c.change = &pendingChange{id: preview.ID, senderID: m.SenderID, expires: time.Now().Add(time.Duration(preview.ExpiresInSeconds) * time.Second)}
	c.manageMu.Unlock()
	slog.Info("management_previewed", "request_id", requestID, "platform", m.Platform, "chat_id", m.ChatID, "command", "config", "operation", preview.Operation, "target", preview.Target)
	c.notify(m.MessageID, fmt.Sprintf("Preview: %s. Confirm with /config confirm %s within %d seconds.", preview.Diff, preview.ID, preview.ExpiresInSeconds))
}
func (c *chat) confirmChange(m Message, client *manage.Client, args []string) {
	if len(args) != 2 {
		c.notify(m.MessageID, "Usage: /config confirm <token>")
		return
	}
	c.manageMu.Lock()
	change := c.change
	c.manageMu.Unlock()
	if change == nil || change.id != args[1] || change.senderID != m.SenderID || time.Now().After(change.expires) {
		c.notify(m.MessageID, "No matching unexpired configuration preview for this chat and sender.")
		return
	}
	requestID := m.RequestID
	if requestID == "" {
		requestID = newRequestID()
	}
	ctx, cancel := c.manageContext()
	defer cancel()
	ctx = manage.WithRequestID(ctx, requestID)
	if err := client.Confirm(ctx, change.id); err != nil {
		slog.Warn("management_failed", "request_id", requestID, "platform", m.Platform, "chat_id", m.ChatID, "command", "config", "error", err)
		c.notify(m.MessageID, manageError(err)+" (request_id="+requestID+")")
		return
	}
	c.manageMu.Lock()
	c.change = nil
	c.manageMu.Unlock()
	slog.Info("management_confirmed", "request_id", requestID, "platform", m.Platform, "chat_id", m.ChatID, "command", "config")
	c.notify(m.MessageID, "Configuration saved. Restart otto serve before using this change.")
}
func (c *chat) cancelChange(m Message, client *manage.Client, args []string) {
	if len(args) != 0 {
		c.notify(m.MessageID, "Usage: /config cancel")
		return
	}
	c.manageMu.Lock()
	change := c.change
	c.change = nil
	c.manageMu.Unlock()
	if change == nil || change.senderID != m.SenderID {
		c.notify(m.MessageID, "No configuration preview is pending for this sender.")
		return
	}
	ctx, cancel := c.manageContext()
	defer cancel()
	if err := client.Cancel(ctx, change.id); err != nil {
		c.notify(m.MessageID, manageError(err))
		return
	}
	c.notify(m.MessageID, "Configuration preview cancelled.")
}
