#!/usr/bin/env bash
# Launch Otto in the foreground with the settings from otto.env.
#
#   scripts/otto.sh [options...]
#
# Sources the launcher configuration (otto.env at the checkout root, or
# OTTO_ENV_FILE as a shell file), translates its settings into the matching
# Otto options, and execs the Otto binary. Anything written after the script
# name is appended last and therefore wins over the file:
#
#   scripts/otto.sh --model other-model --cwd ~/other-project
#
#   OTTO_ENV_FILE  configuration file to read (default <checkout>/otto.env)
#   OTTO_BIN       Otto binary to exec (default <checkout>/otto)
#
# See otto.env.example for the settings and `otto --help` for the options.
set -euo pipefail

root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
otto_bin=${OTTO_BIN:-$root/otto}
env_file=${OTTO_ENV_FILE:-$root/otto.env}

if [[ ! -x $otto_bin ]]; then
  printf 'otto.sh: %s is not executable; run `make build` first.\n' "$otto_bin" >&2
  exit 1
fi

if [[ -f $env_file ]]; then
  # `set -a` so every assignment reaches Otto's environment, in particular the
  # API key variable, which has no command-line option. The file is trusted
  # local configuration and is read like a shell profile.
  set -a
  # shellcheck source=/dev/null
  . "$env_file"
  set +a
else
  printf 'otto.sh: no %s; using Otto configuration and the environment only.\n' "$env_file" >&2
  printf 'otto.sh: copy otto.env.example to %s to set options here.\n' "$env_file" >&2
fi

args=()

# want <option> <value>: append the option unless the setting is empty.
want() {
  if [[ -n ${2:-} ]]; then
    args+=("$1" "$2")
  fi
}

want --provider "${OTTO_PROVIDER:-}"
want --model "${OTTO_MODEL:-}"
want --profile "${OTTO_PROFILE:-}"
want --base-url "${OTTO_BASE_URL:-}"
want --cwd "${OTTO_CWD:-}"
want --ui "${OTTO_UI:-}"
want --sandbox "${OTTO_SANDBOX:-}"
want --thinking "${OTTO_THINKING:-}"
want --shell-timeout "${OTTO_SHELL_TIMEOUT:-}"
want --max-output-bytes "${OTTO_MAX_OUTPUT_BYTES:-}"

if [[ -n ${OTTO_EXTRA_ARGS:-} ]]; then
  read -r -a extra <<<"$OTTO_EXTRA_ARGS"
  args+=("${extra[@]}")
fi

exec "$otto_bin" ${args[@]+"${args[@]}"} "$@"
