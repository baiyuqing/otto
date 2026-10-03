package bridge

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"log/slog"
	"strings"
	"time"

	"github.com/baiyuqing/otto/connect/internal/manage"
)

const manageTimeout = 30 * time.Second

func managementRequestID() string {
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
	c.notify(m.MessageID, "This command requires otto acp --attach connected to a running otto serve.")
	return nil, false
}
func (c *chat) manageContext() (context.Context, context.CancelFunc) {
	return context.WithTimeout(c.b.workCtx, manageTimeout)
}
func manageError(err error) string {
	if api, ok := err.(*manage.APIError); ok {
		switch api.Code {
		case "models_unsupported":
			return "This ChatGPT profile cannot list models; choose an account-available model ID explicitly."
		case "profile_not_found":
			return "Profile not found. Send /config profiles."
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

func (c *chat) models(m Message, name string) {
	go func() {
		client, ok := c.manager(m)
		if !ok {
			return
		}
		if name == "" {
			c.notify(m.MessageID, "Usage: /models <profile>. Send /config profiles to choose a profile.")
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
			c.notify(m.MessageID, "No models were returned for profile "+name+".")
			return
		}
		c.notify(m.MessageID, "Models for "+name+":\n"+strings.Join(ids, "\n"))
	}()
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
			profiles, err := client.Profiles(ctx)
			if err != nil {
				c.notify(m.MessageID, manageError(err))
				return
			}
			var lines []string
			for _, p := range profiles {
				mark := ""
				if p.Default {
					mark = " (default)"
				}
				lines = append(lines, fmt.Sprintf("%s: %s / %s%s", p.Name, p.Provider, p.Model, mark))
			}
			c.notify(m.MessageID, "Profiles:\n"+strings.Join(lines, "\n")+"\nUse /config profiles, /config show <profile>, /models <profile>, or /config set <profile> model <id>.")
			return
		}
		switch args[0] {
		case "profiles":
			profiles, err := client.Profiles(ctx)
			if err != nil {
				c.notify(m.MessageID, manageError(err))
				return
			}
			var lines []string
			for _, p := range profiles {
				mark := ""
				if p.Default {
					mark = " (default)"
				}
				lines = append(lines, fmt.Sprintf("%s: %s / %s%s", p.Name, p.Provider, p.Model, mark))
			}
			c.notify(m.MessageID, strings.Join(lines, "\n"))
		case "show":
			if len(args) != 2 {
				c.notify(m.MessageID, "Usage: /config show <profile>")
				return
			}
			p, err := client.Profile(ctx, args[1])
			if err != nil {
				c.notify(m.MessageID, manageError(err))
				return
			}
			c.notify(m.MessageID, fmt.Sprintf("%s\nprovider: %s\nmodel: %s\nthinking: %s\nbase_url: %s\napi_key_env: %s", p.Name, p.Provider, p.Model, p.Thinking, p.BaseURL, p.APIKeyEnv))
		case "confirm":
			c.confirmChange(m, client, args)
		case "cancel":
			c.cancelChange(m, client, args)
		case "use":
			if len(args) != 2 {
				c.notify(m.MessageID, "Usage: /config use <profile>")
				return
			}
			c.previewChange(m, client, map[string]any{"kind": "set_default_profile", "profile": args[1]})
		case "remove":
			if len(args) != 2 {
				c.notify(m.MessageID, "Usage: /config remove <profile>")
				return
			}
			c.previewChange(m, client, map[string]any{"kind": "remove_profile", "profile": args[1]})
		case "add":
			if len(args) < 6 || len(args)%2 != 0 || args[2] != "--provider" || args[4] != "--model" {
				c.notify(m.MessageID, "Usage: /config add <profile> --provider <openai-compatible|chatgpt> --model <model> [--base-url <url>] [--api-key-env <name>] [--thinking <level>]")
				return
			}
			change := map[string]any{"kind": "create_profile", "profile": args[1], "provider": args[3], "model": args[5]}
			for i := 6; i+1 < len(args); i += 2 {
				switch args[i] {
				case "--base-url":
					change["base_url"] = args[i+1]
				case "--api-key-env":
					change["api_key_env"] = args[i+1]
				case "--thinking":
					change["thinking"] = args[i+1]
				default:
					c.notify(m.MessageID, "Unknown /config add option: "+args[i])
					return
				}
			}
			c.previewChange(m, client, change)
		case "set":
			if len(args) < 4 {
				c.notify(m.MessageID, "Usage: /config set <profile> <model|thinking|base-url|api-key-env> <value>")
				return
			}
			field := strings.ReplaceAll(args[2], "-", "_")
			if field != "model" && field != "thinking" && field != "base_url" && field != "api_key_env" {
				c.notify(m.MessageID, "Only model, thinking, base-url, and api-key-env can be changed.")
				return
			}
			c.previewChange(m, client, map[string]any{"kind": "set_profile_field", "profile": args[1], "field": field, "value": strings.Join(args[3:], " ")})
		default:
			c.notify(m.MessageID, "Usage: /config [profiles|show|use|set|remove|confirm|cancel]")
		}
	}()
}

func (c *chat) previewChange(m Message, client *manage.Client, change any) {
	requestID := managementRequestID()
	ctx, cancel := c.manageContext()
	defer cancel()
	ctx = manage.WithRequestID(ctx, requestID)
	preview, err := client.Preview(ctx, change)
	if err != nil {
		slog.Warn("management_failed", "request_id", requestID, "platform", m.Platform, "chat", m.ChatID, "command", "config", "error", err)
		c.notify(m.MessageID, manageError(err)+" (request_id="+requestID+")")
		return
	}
	c.manageMu.Lock()
	c.change = &pendingChange{id: preview.ID, senderID: m.SenderID, expires: time.Now().Add(time.Duration(preview.ExpiresInSeconds) * time.Second)}
	c.manageMu.Unlock()
	slog.Info("management_previewed", "request_id", requestID, "platform", m.Platform, "chat", m.ChatID, "command", "config", "operation", preview.Operation, "profile", preview.Profile)
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
	requestID := managementRequestID()
	ctx, cancel := c.manageContext()
	defer cancel()
	ctx = manage.WithRequestID(ctx, requestID)
	if err := client.Confirm(ctx, change.id); err != nil {
		slog.Warn("management_failed", "request_id", requestID, "platform", m.Platform, "chat", m.ChatID, "command", "config", "error", err)
		c.notify(m.MessageID, manageError(err)+" (request_id="+requestID+")")
		return
	}
	c.manageMu.Lock()
	c.change = nil
	c.manageMu.Unlock()
	slog.Info("management_confirmed", "request_id", requestID, "platform", m.Platform, "chat", m.ChatID, "command", "config")
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
