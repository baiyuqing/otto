package main

import (
	"bufio"
	"bytes"
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"

	"github.com/baiyuqing/otto/internal/config"
	"github.com/baiyuqing/otto/internal/sandbox"
)

func runSandboxSetup(ctx context.Context, args []string, input io.Reader, out, errOut io.Writer, entries []string, lookup environmentLookup, open func(context.Context, sandboxOpenOptions) sandboxRuntime) int {
	const usage = "usage: otto sandbox setup [--config PATH] [--cwd PATH]"
	if len(args) == 0 || args[0] != "setup" {
		return fail(errOut, usage)
	}
	flags := flag.NewFlagSet("sandbox setup", flag.ContinueOnError)
	flags.SetOutput(io.Discard)
	pathFlag := flags.String("config", "", "configuration file")
	cwd := flags.String("cwd", ".", "workspace directory")
	if err := flags.Parse(args[1:]); err != nil {
		if errors.Is(err, flag.ErrHelp) {
			fmt.Fprintln(out, usage)
			return 0
		}
		return fail(errOut, usage)
	}
	if flags.NArg() != 0 {
		return fail(errOut, usage)
	}
	home, err := resolveHome(lookup, currentOSUserHome)
	if err != nil {
		return fail(errOut, "cannot resolve home directory")
	}
	path := *pathFlag
	if path == "" {
		path = filepath.Join(home, ".config", "otto", "config.toml")
	}
	path, err = filepath.Abs(path)
	if err != nil {
		return fail(errOut, "invalid configuration path")
	}
	original, err := os.ReadFile(path)
	if err != nil && !os.IsNotExist(err) {
		return fail(errOut, "cannot read configuration")
	}
	var file config.File
	if err == nil {
		file, err = config.LoadRequired(path)
		if err != nil {
			return fail(errOut, "configuration is invalid; no changes made")
		}
	}
	workspace, err := canonicalDirectory(*cwd)
	if err != nil {
		return fail(errOut, "workspace is unavailable")
	}
	settings, err := config.ResolveSandbox(file.Sandbox, nil)
	if err != nil {
		return fail(errOut, "sandbox configuration is invalid")
	}
	scanner := bufio.NewScanner(input)
	ask := func(prompt string, defaultYes bool) (bool, bool) {
		suffix := " [y/N]: "
		if defaultYes {
			suffix = " [Y/n]: "
		}
		for {
			fmt.Fprint(out, prompt+suffix)
			if !scanner.Scan() {
				return false, false
			}
			switch strings.ToLower(strings.TrimSpace(scanner.Text())) {
			case "":
				return defaultYes, true
			case "y", "yes":
				return true, true
			case "n", "no":
				return false, true
			default:
				fmt.Fprintln(out, "Enter yes or no.")
			}
		}
	}
	fmt.Fprintf(out, "Sandbox setup for %q\nShell commands can modify the whole workspace. Home files are hidden unless explicitly allowed.\nExisting extra permissions are retained and shown below. Setup enables Seatbelt even if sandbox was off.\n", workspace)
	network, ok := ask("Allow network access?", settings.Network == sandbox.NetworkAllow)
	if !ok {
		return 0
	}
	github, ok := ask("Add GitHub CLI access?", false)
	if !ok {
		return 0
	}
	driver, networkMode := "auto", "deny"
	if network {
		networkMode = "allow"
	}
	proposed := file.Sandbox
	proposed.Driver = &driver
	proposed.Network = &networkMode
	ghDir := ""
	if github {
		ghDir = lookup["GH_CONFIG_DIR"]
		if ghDir == "" {
			ghDir = filepath.Join(home, ".config", "gh")
		}
		if !filepath.IsAbs(ghDir) {
			return fail(errOut, "GH_CONFIG_DIR must be an absolute directory; no changes made")
		}
		ghDir, err = canonicalDirectory(ghDir)
		if err != nil {
			return fail(errOut, "GitHub configuration directory is missing; run gh auth login outside Otto, then retry (set GH_CONFIG_DIR for a custom location)")
		}
		if !slices.Contains(proposed.ReadPaths, ghDir) {
			proposed.ReadPaths = append(slices.Clone(proposed.ReadPaths), ghDir)
		}
		if !slices.Contains(proposed.AllowEnv, "GH_CONFIG_DIR") {
			proposed.AllowEnv = append(slices.Clone(proposed.AllowEnv), "GH_CONFIG_DIR")
		}
		fmt.Fprintln(out, "GitHub CLI access may expose saved GitHub credentials to shell commands. Network access allows those commands to send data externally.")
	}
	updated, err := config.UpdateSandbox(original, proposed)
	if err != nil {
		return fail(errOut, "%v", err)
	}
	fmt.Fprintf(out, "\nConfiguration: %q (applies to future Otto processes using this file)\nDriver: auto (Seatbelt)\nNetwork: %s\nRead paths: %q\nAllowed environment names: %q\n", path, networkMode, proposed.ReadPaths, proposed.AllowEnv)
	launch := func() {
		if github {
			fmt.Fprintf(out, "Start Otto with: GH_CONFIG_DIR=%s otto --config %s --cwd %s\n", shellQuoteSetup(ghDir), shellQuoteSetup(path), shellQuoteSetup(workspace))
		}
	}
	launch()
	for {
		fmt.Fprint(out, "[check / save / cancel] (cancel): ")
		if !scanner.Scan() {
			return 0
		}
		switch strings.ToLower(strings.TrimSpace(scanner.Text())) {
		case "", "cancel":
			fmt.Fprintln(out, "Cancelled; configuration unchanged.")
			return 0
		case "check":
			checkEntries := slices.Clone(entries)
			if github {
				checkEntries = slices.DeleteFunc(checkEntries, func(s string) bool { return strings.HasPrefix(s, "GH_CONFIG_DIR=") })
				checkEntries = append(checkEntries, "GH_CONFIG_DIR="+ghDir)
			}
			resolved, _ := config.ResolveSandbox(proposed, nil)
			checkSandboxSetup(ctx, open, sandboxOpenOptions{Settings: resolved, Workspace: workspace, Shell: "/bin/bash", Home: home, HostEntries: checkEntries, ProviderNames: sandboxProviderEnvironmentNames(file, "")}, github, out)
		case "save":
			if err := saveSandboxSetup(path, original, updated); err != nil {
				return fail(errOut, "cannot save configuration: %v", err)
			}
			fmt.Fprintln(out, "Saved. Restart Otto to apply these permissions.")
			launch()
			return 0
		default:
			fmt.Fprintln(out, "Enter check, save, or cancel.")
		}
	}
}

func shellQuoteSetup(s string) string { return "'" + strings.ReplaceAll(s, "'", "'\"'\"'") + "'" }

func checkSandboxSetup(ctx context.Context, open func(context.Context, sandboxOpenOptions) sandboxRuntime, options sandboxOpenOptions, github bool, out io.Writer) {
	ctx, cancel := context.WithTimeout(ctx, 15*time.Second)
	defer cancel()
	runtime := open(ctx, options)
	defer func() {
		if runtime.close != nil {
			if runtime.close() != nil {
				fmt.Fprintln(out, "Sandbox cleanup failed.")
			}
		}
	}()
	if !runtime.Info.BashAvailable || runtime.Executor == nil {
		fmt.Fprintf(out, "Sandbox startup failed (%s). Check read paths and Seatbelt availability; permissions were not widened.\n", runtime.Info.Reason)
		return
	}
	command := "true"
	if github {
		command = `command -v gh >/dev/null || exit 20; test -d "$GH_CONFIG_DIR" && test -r "$GH_CONFIG_DIR" || exit 21; gh --version >/dev/null 2>&1 || exit 22`
	}
	status, err := runtime.Executor.Execute(ctx, sandbox.Request{Argv: []string{"/bin/bash", "-c", command}, Dir: options.Workspace, Env: runtime.Environment}, sandbox.Streams{Stdout: io.Discard, Stderr: io.Discard})
	if err != nil {
		fmt.Fprintln(out, "Sandbox check could not execute or timed out.")
		return
	}
	switch status.Code {
	case 0:
		fmt.Fprintln(out, "Sandbox check passed. GitHub authentication and network connectivity were not tested.")
	case 20:
		fmt.Fprintln(out, "GitHub CLI is not on the sandbox PATH. Install gh or check PATH.")
	case 21:
		fmt.Fprintln(out, "GitHub configuration directory is not readable inside the sandbox.")
	default:
		fmt.Fprintln(out, "Command failed inside the sandbox. This does not by itself establish a permission or authentication problem.")
	}
}

func saveSandboxSetup(path string, original, updated []byte) error {
	current, err := os.ReadFile(path)
	if err != nil && !os.IsNotExist(err) {
		return errors.New("cannot read current file")
	}
	if !bytes.Equal(current, original) {
		return errors.New("configuration changed during setup; rerun setup")
	}
	if info, err := os.Lstat(path); err == nil && !info.Mode().IsRegular() {
		return errors.New("configuration must be a regular file, not a symlink")
	}
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return errors.New("cannot create configuration directory")
	}
	temp, err := os.CreateTemp(filepath.Dir(path), ".otto-config-*")
	if err != nil {
		return errors.New("cannot create temporary configuration")
	}
	defer os.Remove(temp.Name())
	_, writeErr := temp.Write(updated)
	closeErr := temp.Close()
	if writeErr != nil || closeErr != nil {
		return errors.New("cannot write configuration")
	}
	if err := os.Rename(temp.Name(), path); err != nil {
		return errors.New("cannot replace configuration")
	}
	return nil
}
