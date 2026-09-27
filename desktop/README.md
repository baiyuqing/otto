# Otto desktop shell

A macOS Tauri 2 window around `otto serve`. It is a client of the HTTP API
documented in [the user manual's Agent server
section](../docs/user-manual.md#agent-server); it adds no server behavior of
its own. See [the user manual's Desktop app
section](../docs/user-manual.md#desktop-app-macos) for what the app does at
startup, on **File > Open Folder…**, and on quit.

`desktop/src-tauri` is its own Cargo workspace (`[workspace]` in its
`Cargo.toml`), so it is not a member of the root Otto workspace and is not
built or tested by `make check`/`make check-linux`.

## Build

Tauri's build script requires the `otto` sidecar binary to exist on disk,
named for its target triple, before `cargo build` runs in this directory:

```bash
cargo build --release --target aarch64-apple-darwin -p otto   # from the repo root
mkdir -p desktop/src-tauri/binaries
cp target/aarch64-apple-darwin/release/otto \
  desktop/src-tauri/binaries/otto-aarch64-apple-darwin
cd desktop/src-tauri
cargo build
```

The app is arm64-only (`aarch64-apple-darwin`); there is no Intel build.

## Check

```bash
make desktop-check   # from the repo root
```

Builds the release `otto` binary, copies it to the sidecar path above, then
runs `cargo fmt --check`, `cargo clippy -D warnings`, and `cargo test`
(unit tests plus an integration test that starts a real `otto serve`
process, hits `/healthz`, and checks it exits within 5 seconds of its stdin
closing and of receiving `SIGTERM`) inside `desktop/src-tauri`. Not part of
`make check`.

The integration test binds a loopback TCP port; an agent sandbox that denies
local port binding needs that restriction lifted to run it.

## Release

```bash
APPLE_SIGNING_IDENTITY="Developer ID Application: ..." \
NOTARY_PROFILE=otto-notary \
make desktop-release
```

Builds and copies the sidecar binary, runs `cargo tauri build` with hardened
runtime and the given signing identity, then `xcrun notarytool submit
--wait` and `xcrun stapler staple` on the built `.app`. Fails immediately,
before building anything, if either `APPLE_SIGNING_IDENTITY` or
`NOTARY_PROFILE` is unset. `NOTARY_PROFILE` names a keychain profile created
with `xcrun notarytool store-credentials`.

This target needs `cargo-tauri` (`cargo install tauri-cli --version ^2`,
not installed by any other Makefile target) and a Developer ID Application
identity in the local keychain; neither is available in every environment
this repository is checked out in.

## Locations

| What | Path |
| --- | --- |
| Saved state (last opened folder) | `~/Library/Application Support/com.otto.desktop/state.json` |
| Child process log (`otto serve`'s stderr) | `~/Library/Logs/com.otto.desktop/serve.log` |
