# macOS desktop shell

Status: approved 2026-09-27.

Step 5 of the multi-session plan: a macOS app that runs `otto serve` as a
child process and shows the existing Web UI in a native window. Steps 1-4
(multi-workspace serve, sidebar by working directory, status stream, diff
review) are merged.

Where a question has an answer in OpenAI Codex's public repository
(`openai/codex`, `codex-rs/`), this design follows it. Codex's desktop app
itself is closed source; the comparison is with `codex app-server`, the
backend its GUI clients run.

## Scope

Two pull requests.

- **PR A, `otto`:** per-directory trust (`otto trust`) as a second admission
  rule, and `otto serve --exit-on-stdin-close`.
- **PR B, `desktop/`:** the Tauri app, `make desktop-check`, and
  `make desktop-release`.

Not in scope: Intel and universal builds (arm64 only), auto-update, a Mac App
Store build, the App Sandbox, opening the app from the terminal with a
directory argument (Codex's `codex app <path>`), trust affecting approval or
sandbox policy, and Linux or Windows builds.

## PR A: changes to `otto`

### 1. Per-directory trust

Today a workspace other than the startup workspace is admitted only when it
is under one of `[server].workspace_roots`. A directory the user picks in the
desktop app is usually not under a configured root.

Codex records trust per directory in `config.toml`
(`[projects."<path>"] trust_level = "trusted"`). Otto adopts the same shape:

```toml
[projects."/Users/me/src/app"]
trust_level = "trusted"
```

- `otto trust <dir>` canonicalizes `<dir>` (it must be an existing
  directory) and appends that table to the config file. If the table is
  already there with `trust_level = "trusted"`, the command writes nothing.
  The file is changed by appending text, not by a schema round trip, so
  comments are kept. The write goes through `config::write_bytes`, so it
  takes a backup and refuses when another process changed the file in
  between.
- `trust_level` accepts only `"trusted"`. Any other value is a config error
  naming the key.
- Admission becomes: the startup workspace, a descendant of a
  `workspace_roots` entry, or a trusted directory or its descendant. The
  result is still `NotAdmitted` or `Invalid` as today.
- A running `otto serve` re-reads the `[projects]` tables from the config
  file on every admission (each `POST /v1/workspaces` and session create
  that names a workspace). A directory trusted after the server started is
  therefore admitted without a restart. A trusted path that no longer
  resolves is skipped. Unlike a root, it is not a startup error.
- There is no HTTP route that adds trust. The token lets a client load
  workspaces inside what the user has already allowed; it must not widen
  that set. Only a local process running as the user (the CLI, or the
  desktop app running the CLI) writes trust.
- Trust affects admission only. Codex also uses trust to choose approval
  and sandbox defaults; otto keeps one sandbox and approval policy for
  every workspace.

### 2. `otto serve --exit-on-stdin-close`

Codex's `app-server` ends when its stdin reaches EOF or on SIGTERM. With the
flag, `otto serve` reads stdin and, on EOF or a read error, cancels the same
token SIGTERM cancels. Shutdown then follows the existing path. The desktop
app keeps the write end of the child's stdin open for its whole life. When
the app exits or crashes, the kernel closes that pipe and the child shuts
down.

Without the flag, stdin is not read, so `otto serve < /dev/null` in a
script behaves as today. The flag requires a TCP listener (`--listen` or
`[server].listen`), like `--open`.

### PR A tests

1. `otto trust <dir>` appends the table and keeps existing comments. A
   second run writes nothing.
2. `otto trust` on a missing path or a file fails and does not write.
3. `otto trust` refuses when the file changed between read and write (the
   existing compare-and-swap).
4. Admission: a trusted directory outside every root and a descendant of it
   are admitted. A sibling with a shared name prefix (`/a/app-x` for trusted
   `/a/app`) is not.
5. A directory trusted after the server started is admitted without a
   restart. A trusted path that no longer exists is skipped.
6. `trust_level = "untrusted"` is a config error naming the key.
7. `--exit-on-stdin-close`: EOF on the reader cancels the serve token. Also
   tested: no flag means stdin is not read, and the flag with a Unix socket
   is rejected.

Docs: the user manual (`otto trust`, `[projects]`, the admission rule, the
new flag) and the config reference.

## PR B: `desktop/`

### Layout and build

- `desktop/` is a Tauri 2 project with its own Cargo workspace. It is not a
  member of the root workspace, so `make check` and CI do not build it.
- The bundle carries the `otto` binary as a Tauri `externalBin`
  (`Contents/MacOS/otto`), built from the same checkout with
  `cargo build --release -p otto --target aarch64-apple-darwin`.
- The window loads the page that the child's `otto serve` serves. It does
  not bundle `ui/dist` separately.

### Startup

1. **Environment.** A Finder or Dock launch does not inherit the variables
   set in shell rc files, so `api_key_env` variables and the user's `PATH`
   are missing. Codex resolves the user's shell from the password database
   (`getpwuid_r`, not `$SHELL`) and captures its environment with `env -0`.
   The app does the same: it runs `<shell> -l -i -c 'printf OTTO_ENV_BEGIN;
   env -0'` with a 10 s timeout, and parses the NUL-separated pairs after
   the marker, because rc files can print to stdout first. On a timeout or
   parse failure it uses its own environment and shows a notice in the
   window. The captured environment is passed only to the child; the app
   does not log it.
2. **Startup directory.** On first launch, the app shows a native folder
   picker. It runs `otto trust <dir>` on the chosen directory and saves the
   path to `~/Library/Application Support/<bundle id>/state.json`. Later
   launches use the saved directory. If it no longer exists, the picker is
   shown again.
3. **Child.** The app runs `otto serve --cwd <dir> --listen 127.0.0.1:0
   --exit-on-stdin-close`, with the captured environment, stdin as a pipe
   the app holds, stdout as a pipe, and stderr appended to
   `~/Library/Logs/<bundle id>/serve.log`. It reads stdout lines until one
   starts with `otto serve: ` (30 s timeout). If the child exits before
   that line, the window shows the child's exit status and the last 50
   lines of `serve.log`.
4. **Window.** The webview navigates to the URL from that line. The Web UI
   already moves `?token=` into `sessionStorage` and removes it from the
   address. The remote origin gets no Tauri IPC capability: the page stays
   a plain HTTP client of `otto serve`, as in a browser.

### Adding a directory

The app menu gets **File > Open Folder… (⌘O)**. It shows the native folder
picker, runs `otto trust <dir>`, calls `POST /v1/workspaces` itself with the
token, and reloads the webview. The token in `sessionStorage` survives the
reload. The picker runs in the native process, outside the page, so trust
is granted only by a user action in the app. The Web UI's **Add workspace**
field keeps working for directories under a root or already trusted.

### Shutdown

Closing the last window quits the app. On quit, the app sends SIGTERM to the
child, waits up to 10 s, then sends SIGKILL. If the app is killed instead,
the stdin pipe closes and the child's `--exit-on-stdin-close` handles it.

### Signing and notarization

`make desktop-release` runs only on a developer's Mac. It builds, signs with
the Developer ID Application identity named by `APPLE_SIGNING_IDENTITY`,
enables the hardened runtime, notarizes with `xcrun notarytool
--keychain-profile <name>` (credentials stay in the login keychain), and
staples the ticket. CI gets no Apple credentials. No entitlement beyond the
hardened runtime defaults is requested unless the first check below needs
one.

The first task of PR B, before anything else, is this check: from a signed,
hardened-runtime, notarized build, `bash` runs under Seatbelt
(`/usr/bin/sandbox-exec`). This has to hold for both the bundled `otto` and
the `sandbox-exec` it starts. Codex's sandbox code has no branch for a
signed or hardened app, which suggests no special case is needed, but no
code in Codex's public repository shows it. If the check fails, PR B stops
and the result comes back for a decision.

### `make desktop-check`

Not part of `make check`. It runs on macOS:

1. `cargo fmt --check` and `cargo clippy -D warnings` in `desktop/`.
2. Unit tests: the `otto serve: ` line parser (it skips other lines and
   times out); the `env -0` parser (it skips output before the marker,
   handles a value containing `=` or a newline, and rejects output with no
   marker).
3. An integration test that runs the release `otto` from this checkout
   with a temporary `HOME` and a test config. It checks the flags the app
   passes, `GET /healthz` on the reported port, that closing stdin makes
   the child exit within 5 s, and that SIGTERM does the same.

These tests bind a loopback port and use no network or credentials.

### Manual acceptance (PR description)

On the signed build:

- first-launch picker;
- a Finder launch finds an `api_key_env` key set in `~/.zshrc`;
- Open Folder adds a directory outside every root;
- two sessions in different directories run turns concurrently;
- quitting leaves no `otto` process;
- `kill -9` of the app leaves no `otto` process after 5 s;
- `bash` is confined by Seatbelt.

Docs: a user manual section for the desktop app, limited to what the
acceptance list verified; `desktop/README.md` for building and signing.
