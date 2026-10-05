# Docker on Mac

Status: implemented; approved by the user on 2026-10-05.

## Goal

Build Otto from source and run its CLI and complete Web UI through Docker
Desktop on Apple Silicon and Intel Macs. The container runs Linux; the native
macOS desktop shell and Seatbelt are outside this scope.

## Packaging and execution

- Add a multi-stage Dockerfile: pinned Rust toolchain, Node 24, wasm-pack,
  WASM bindings and UI first, then the release `otto` binary with the UI
  embedded. Use architecture-neutral base images for linux/arm64 and
  linux/amd64. Keep build tools outside the runtime image.
- Run as a non-root user. Include CA certificates, Bash, Git and the bundled
  sandbox-setup skill. Keep API keys in runtime environment variables only.
- Support CLI commands with `docker run --rm -it`, workspace bind mounts,
  and a named volume for the container user's home, covering `.config/otto`
  configuration and `.otto` state.
- Add a Docker Compose setup for the Web UI. Otto listens at
  `127.0.0.1:8787` inside its container. A TCP relay in a separate container
  sharing Otto's network namespace forwards port 8788 to that listener.
  Publish only Mac loopback port `127.0.0.1:8787` to container port 8788.
  Preserve Otto's existing per-process bearer token and startup URL; no
  native listener or authentication changes are required.
- Keep the existing sandbox default. Document that Bash is unavailable in
  Linux containers unless the user explicitly passes `--sandbox off`.
  Container isolation does not supply Seatbelt protection for mounted files.
- Exclude Git metadata, credentials, generated artifacts, worktrees and
  local state from the Docker build context with `.dockerignore`.
- Update English/Chinese READMEs, the user manual and changelog with build,
  startup, token discovery, provider configuration, data persistence,
  shutdown and architecture selection instructions.

## Acceptance

Validate Compose configuration, build the complete image, verify CLI startup,
embedded UI delivery, unauthenticated API rejection, authenticated health,
workspace write access, state persistence and graceful shutdown without a
live provider. Run focused checks and `make check-fast`. Validate both image
architectures where the environment supports execution; explicitly report
any architecture or Docker Desktop checks that cannot run here.

Do not publish images or add the desktop shell or chat connector to this
initial image. Prepare a PR after implementation if GitHub authentication is
available.

## Validation record

The Linux amd64 release image was built and its non-root CLI, real setup
flow, embedded JavaScript/WASM, token enforcement and rotation, named-session
persistence, workspace writes and graceful shutdown passed the opt-in Docker
smoke test. Compose configuration, the ARM64 Docker build configuration
check and `make check-fast` passed. ARM64 execution and actual Docker Desktop
file sharing on Mac were not available in the cloud executor.
