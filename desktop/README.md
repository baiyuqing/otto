# Otto desktop shell

A macOS [MyGo](https://github.com/egoist/mygo) window around the Rust
`otto serve` process. The existing web UI is served by Otto, not embedded
separately in the Go shell. The shell adds no agent or server behavior.
See the [Desktop app section](../docs/user-manual.md#desktop-app-macos)
for startup, **File > Open Folder…**, and quit behavior.

`desktop/` is an independent Go module. `main_darwin.go` owns the native
window, menus and dialogs; `internal/shell` owns environment capture,
saved state, process lifecycle, URL validation and workspace HTTP calls.
MyGo is pinned in `go.mod` and `go.sum`; its CLI uses the same module version.

## Web UI bridge

The main window loads `http://127.0.0.1:<port>`. A document-start script
provides `window.__OTTO_DESKTOP__.openFolder()`, used by **Add workspace…**.
It navigates to `/__otto_desktop/open-folder`; the same-origin navigation
handler cancels it and runs the native folder picker, `otto trust`, an
app-owned authenticated `POST /v1/workspaces`, and a page reload.

The shell binds no Go services and grants no additional trusted origins.
Workspace HTTP requests use a proxy-free client and never follow redirects.
All blocking startup, folder selection and process cleanup runs off the
native main loop. Quitting during startup cancels pending commands and
cleans up a child even if it was spawned concurrently with quit.

## Build Otto.app

Prerequisites on an Apple silicon Mac:

- Go 1.27.1 or newer, plus the repository's pinned Rust toolchain.
- Node 24+, npm and wasm-pack for the existing Web UI build.
- Standalone Apple Command Line Tools for signing and packaging.

```bash
make desktop-app
```

This builds the Web UI and Rust `otto` for `aarch64-apple-darwin`, copies
it to `desktop/resources/darwin-arm64/bin/otto`, and runs the pinned MyGo
CLI to build `desktop/build/darwin-arm64/Otto.app` with an ad-hoc signature.
MyGo packages and signs the Rust helper under `Contents/Resources/bin/otto`.
The app remains macOS arm64 only; no additional platform support is claimed.
No Bun, separate frontend build for MyGo, or globally installed MyGo CLI
is needed: the configuration is `mygo.json` and the UI comes from Otto.

A locally built app carries no quarantine attribute and can open on its
build Mac. For distribution, use the signed release target below.

## Check

```bash
make desktop-check
```

On macOS or Linux, this builds a native debug `otto` using the shared Rust
test build, checks Go formatting and vet, runs race-enabled Go tests with
`OTTO_BIN=target/debug/otto`, and cross-compiles/vets the macOS arm64 shell
without cgo. The real-server tests check `/healthz`, authenticated workspace
registration, stdin-close exit, and SIGTERM exit within five seconds.
Other tests cover login-shell output parsing, probe timeout, state format
and locations, startup URL validation, navigation origin restrictions,
proxy/redirect isolation, spawn arguments/environment, and child cleanup.

`make check-fast` selects this gate when desktop files change; CI runs it
in `make check` and `make check-linux`. Loopback binding must be permitted
by the execution environment. To run just the offline Go tests:

```bash
cd desktop
go test -race ./...
```

The real-server tests skip unless `OTTO_BIN` is set to an absolute binary
path. Cross-compilation does not verify native windows, directory dialogs,
Finder login-shell capture, signing or notarization; validate these on a Mac.

## Release

```bash
APPLE_SIGNING_IDENTITY="Developer ID Application: ..." \
NOTARY_PROFILE=otto-notary \
make desktop-release
```

The target checks both variables before building, builds the UI and sidecar,
and uses MyGo to sign the app and helper with hardened runtime. It then
archives the `.app` with `ditto`, submits it through `xcrun notarytool
--wait`, and staples the ticket onto the app. `NOTARY_PROFILE` names a
keychain profile created with `xcrun notarytool store-credentials`.
The Developer ID identity and profile must already exist in the Mac's keychain.

## Locations

The bundle identifier and existing user paths are preserved on upgrade.
MyGo's app-name-based default directories are not used for saved state/logs.

| What | Path |
| --- | --- |
| Saved workspace | `~/Library/Application Support/com.otto.desktop/state.json` |
| Server stderr (appended) | `~/Library/Logs/com.otto.desktop/serve.log` |
| Bundled Rust server | `Otto.app/Contents/Resources/bin/otto` |
