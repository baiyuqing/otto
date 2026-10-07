#!/usr/bin/env bash
# Run the direct ACP TCP deployment: one `otto serve --acp-listen HOST:PORT`
# and one `otto-connect` whose connect.toml uses `[agent] address`, supervised
# as a pair. Connect starts no `otto acp` child in this mode; the two
# processes talk over the authenticated loopback TCP listener of serve.
#
# Settings come from otto.env (default <checkout>/otto.env, override with
# OTTO_ENV_FILE), the same file scripts/otto.sh reads, plus the settings in
# OTTO_TCP_* below. Secrets are environment variables: OTTO_ACP_TOKEN is read
# by `otto serve` and by connect through `[agent] token_env`, and the
# connector's Telegram token or Feishu app secret come from the variables its
# configuration names. The script writes no secret.
#
# The supervisor restarts a process that exits, 1 s doubling to 60 s, and
# immediately after it had run for 60 s; five consecutive fast failures stop
# the whole stack instead of looping. SIGUSR1 restarts serve, SIGUSR2 restarts
# connect, SIGTERM and SIGINT stop the stack. See the user manual.
set -euo pipefail
umask 077

PROGRAM=otto-tcp

die() {
  printf '%s: %s\n' "$PROGRAM" "$1" >&2
  exit "${2:-1}"
}

warn() { printf '%s: %s\n' "$PROGRAM" "$1" >&2; }

log() {
  printf '%s %s: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "$PROGRAM" "$1" >&2
}

usage() {
  cat <<'EOF'
otto-tcp.sh runs the direct ACP TCP deployment: one `otto serve
--acp-listen HOST:PORT` plus one `otto-connect` configured with
`[agent] address`, so connect starts no `otto acp` process.

  otto-tcp.sh start                    start the stack in the background
  otto-tcp.sh stop                     stop both processes
  otto-tcp.sh restart [serve|connect]  restart one process, or the stack
  otto-tcp.sh status                   what is running, and where state lives
  otto-tcp.sh logs [serve|connect|supervisor]
  otto-tcp.sh run                      run the supervisor in the foreground
  otto-tcp.sh help

Settings come from otto.env (OTTO_ENV_FILE) and the environment:
OTTO_BIN, OTTO_CONNECT_BIN, OTTO_ACP_LISTEN, OTTO_CONNECT_CONFIG, OTTO_CWD,
OTTO_LISTEN, OTTO_SERVE_EXTRA_ARGS, OTTO_TCP_HOME. OTTO_ACP_TOKEN and the
connector's platform secrets are environment-only. Defaults and the advanced
OTTO_TCP_* settings are documented in the user manual.
EOF
}

now() { date +%s; }

# ---- settings ----

self=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)/$(basename -- "${BASH_SOURCE[0]}")
root=$(cd -- "$(dirname -- "$self")/.." && pwd)
env_file=${OTTO_ENV_FILE:-$root/otto.env}

# otto.env is read like a shell profile, exactly as scripts/otto.sh reads it,
# so quoting, `#` comments and `export` work. `set -a` puts every assignment
# into the environment of the two child processes, which is also how a key or
# token placed in the file reaches them. Command-line environment variables of
# the caller are overridden by the file, like the launcher.
if [[ -f $env_file ]]; then
  set -a
  # shellcheck source=/dev/null
  . "$env_file"
  set +a
else
  printf '%s: no %s; using Otto configuration and the environment only.\n' "$PROGRAM" "$env_file" >&2
fi

state_dir=${OTTO_TCP_HOME:-${XDG_STATE_HOME:-$HOME/.local/state}/otto-tcp}
supervisor_pid_file=$state_dir/supervisor.pid
serve_pid_file=$state_dir/serve.pid
connect_pid_file=$state_dir/connect.pid
supervisor_log=$state_dir/supervisor.log
serve_log=$state_dir/serve.log
connect_log=$state_dir/connect.log

acp_listen=${OTTO_ACP_LISTEN:-127.0.0.1:9001}
connect_config=${OTTO_CONNECT_CONFIG:-$HOME/.config/otto/connect.toml}
poll=${OTTO_TCP_POLL:-1}
ready_timeout=${OTTO_TCP_READY_TIMEOUT:-20}
stop_timeout=${OTTO_TCP_STOP_TIMEOUT:-20}
min_delay=${OTTO_TCP_MIN_DELAY:-1}
max_delay=${OTTO_TCP_MAX_DELAY:-60}
stable_after=${OTTO_TCP_STABLE_AFTER:-60}
max_fails=${OTTO_TCP_MAX_FAILS:-5}
skip_checks=${OTTO_TCP_SKIP_CHECKS:-}

otto_bin=${OTTO_BIN:-}
if [[ -z $otto_bin ]]; then
  if [[ -x $root/otto ]]; then otto_bin=$root/otto; else otto_bin=$(command -v otto || true); fi
fi
connect_bin=${OTTO_CONNECT_BIN:-}
if [[ -z $connect_bin ]]; then
  if [[ -x $root/target/otto-connect ]]; then connect_bin=$root/target/otto-connect; else connect_bin=$(command -v otto-connect || true); fi
fi

# ---- small helpers ----

alive() { kill -0 "$1" 2>/dev/null; }

file_size() {
  if [[ -f $1 ]]; then wc -c <"$1" | tr -d ' '; else printf '0'; fi
}

# new_since FILE OFFSET PATTERN: true when the bytes written after OFFSET
# contain PATTERN. Reading from the recorded offset keeps a line from an
# earlier run, or from an earlier start in the same file, from counting as
# readiness. The `|| true` keeps a tail that loses the race with an exiting
# grep from failing the pipeline under `pipefail`.
new_since() {
  local size
  size=$(file_size "$1")
  [[ $size -gt $2 ]] || return 1
  { tail -c +$(( $2 + 1 )) "$1" 2>/dev/null || true; } | grep -q -F -- "$3"
}

read_pid() {
  local pid
  [[ -f $1 ]] || return 1
  pid=$(cat "$1" 2>/dev/null || true)
  case $pid in
  '' | *[!0-9]*) return 1 ;;
  esac
  printf '%s' "$pid"
}

# live_pid_of FILE PATTERN: prints the pid when the file names a live process
# whose command line matches PATTERN. The command check keeps a recycled pid
# from being signalled as if it were part of this stack; a platform or sandbox
# that hides `ps` (it reads kernel state) only disables that check, because a
# command line that could not be read is not a mismatch.
live_pid_of() {
  local pid command
  pid=$(read_pid "$1") || return 1
  alive "$pid" || return 1
  if [[ -n ${2:-} ]]; then
    command=$(ps -p "$pid" -o command= 2>/dev/null || true)
    case $command in
    '') ;;
    *"$2"*) ;;
    *) return 1 ;;
    esac
  fi
  printf '%s' "$pid"
}

uptime_of() {
  local value
  value=$(ps -p "$1" -o etime= 2>/dev/null | tr -d ' ' || true)
  printf '%s' "${value:-unknown}"
}

# toml_table_key FILE TABLE KEY: the value of KEY in the [TABLE] table, with
# surrounding double quotes removed. Only the simple `key = "value"` form this
# project's configuration uses is read; anything else yields nothing.
toml_table_key() {
  awk -v table="$1" -v key="$2" '
    /^[ \t]*\[/ { section = $0; gsub(/[ \t]/, "", section); next }
    section == "[" table "]" && $0 ~ ("^[ \t]*" key "[ \t]*=") {
      line = $0
      sub(/^[^=]*=[ \t]*/, "", line)
      sub(/[ \t]+$/, "", line)
      if (line ~ /^".*"$/) line = substr(line, 2, length(line) - 2)
      print line
      exit
    }' "$3"
}

# host_port VALUE: splits VALUE into the globals HP_HOST and HP_PORT. False
# when VALUE carries no port.
host_port() {
  case $1 in
  \[*\]:*) HP_HOST=${1%%]*}; HP_HOST=${HP_HOST#[}; HP_PORT=${1##*:} ;;
  *:*) HP_HOST=${1%:*}; HP_PORT=${1##*:} ;;
  *) return 1 ;;
  esac
  return 0
}

loopback_address() {
  host_port "$1" || return 1
  case $HP_HOST in
  127.0.0.1 | localhost | ::1) ;;
  *) return 1 ;;
  esac
  case $HP_PORT in
  '' | *[!0-9]*) return 1 ;;
  esac
  [[ $HP_PORT -ge 1 && $HP_PORT -le 65535 ]] || return 1
  return 0
}

integer() {
  case ${1:-} in
  '' | *[!0-9]*) return 1 ;;
  esac
  return 0
}

seconds() {
  case ${1:-} in
  '' | *[!0-9.]*) return 1 ;;
  esac
  return 0
}

canonical_dir() {
  local path=$1
  if [[ -d $path ]]; then (cd -- "$path" && pwd -P); else printf '%s' "$path"; fi
}

# resolve_workspace: the workspace serve starts in and every connect session
# uses as its cwd. It comes from OTTO_CWD or from connect.toml, and serve needs
# an absolute path either way.
resolve_workspace() {
  [[ -n $workspace ]] || die "no workspace: set OTTO_CWD in otto.env, or [agent].workspace in $connect_config"
  [[ $workspace == /* ]] || die "the serve workspace must be an absolute path, got '$workspace'"
}

# ---- preflight ----

preflight() {
  local name value
  for name in poll ready_timeout stop_timeout min_delay max_delay stable_after max_fails; do
    eval "value=\${$name}"
    if [[ $name == poll || $name == min_delay || $name == max_delay ]]; then
      seconds "$value" || die "OTTO_TCP_$(printf '%s' "$name" | tr 'a-z' 'A-Z') must be a number, got '$value'"
    else
      integer "$value" || die "OTTO_TCP_$(printf '%s' "$name" | tr 'a-z' 'A-Z') must be a whole number, got '$value'"
    fi
  done
  [[ -x $otto_bin ]] || die "no Otto binary at '${otto_bin:-otto}'; run make build or set OTTO_BIN"
  [[ -x $connect_bin ]] || die "no otto-connect binary at '${connect_bin:-otto-connect}'; run make connect-build or set OTTO_CONNECT_BIN"

  local token=${OTTO_ACP_TOKEN:-}
  [[ -n $token ]] || die "OTTO_ACP_TOKEN is not set; both otto serve and otto-connect read it in this mode"
  case $token in
  *[![:print:]]* | *' '*) die "OTTO_ACP_TOKEN must be printable ASCII without spaces" ;;
  esac
  [[ ${#token} -le 4000 ]] || die "OTTO_ACP_TOKEN must be at most 4000 bytes"

  loopback_address "$acp_listen" || die "OTTO_ACP_LISTEN must be a loopback host:port, got '$acp_listen'"
  if [[ -n ${OTTO_LISTEN:-} ]]; then
    loopback_address "$OTTO_LISTEN" || die "OTTO_LISTEN must be a loopback host:port, got '$OTTO_LISTEN'"
  fi
  [[ -f $connect_config ]] || die "connect configuration '$connect_config' not found; set OTTO_CONNECT_CONFIG or write the [agent] address section"
  check_connect_config
}

# check_connect_config fails before either process starts on a configuration
# that cannot work: connect would start its own `otto acp` (the mode this
# script exists to avoid), talk to another port, or fail authentication.
# OTTO_TCP_SKIP_CHECKS turns these checks off for a configuration whoever runs
# the stack vouches for, for example one this reader cannot parse; the
# workspace is always resolved, because serve needs it.
check_connect_config() {
  local address token_env command workspace_cfg
  address=$(toml_table_key agent address "$connect_config")
  token_env=$(toml_table_key agent token_env "$connect_config")
  command=$(toml_table_key agent command "$connect_config")
  workspace_cfg=$(toml_table_key agent workspace "$connect_config")

  if [[ -n $skip_checks ]]; then
    warn "skipping the [agent] checks (OTTO_TCP_SKIP_CHECKS is set); connect.toml must already use address = \"$acp_listen\" and token_env = \"OTTO_ACP_TOKEN\""
  else
    if [[ -n $command ]]; then
      die "connect.toml sets [agent].command; connect would start its own otto acp. Remove it, because address and command are mutually exclusive"
    fi
    if [[ -z $address ]]; then
      die "connect.toml has no [agent].address; add address = \"$acp_listen\" and token_env = \"OTTO_ACP_TOKEN\""
    fi
    if [[ $address != "$acp_listen" ]]; then
      if ! loopback_address "$address"; then
        die "connect.toml [agent].address '$address' is not a loopback host:port"
      fi
      local a_host a_port b_host b_port
      host_port "$address"
      a_host=$HP_HOST
      a_port=$HP_PORT
      host_port "$acp_listen"
      b_host=$HP_HOST
      b_port=$HP_PORT
      case $a_host in localhost) a_host=127.0.0.1 ;; esac
      case $b_host in localhost) b_host=127.0.0.1 ;; esac
      if [[ $a_host != "$b_host" || $a_port != "$b_port" ]]; then
        die "connect.toml [agent].address is '$address' but this stack serves ACP on '$acp_listen'; set OTTO_ACP_LISTEN=$address or edit the configuration"
      fi
    fi
    if [[ -z $token_env ]]; then
      die "connect.toml has no [agent].token_env; set token_env = \"OTTO_ACP_TOKEN\""
    fi
    if [[ $token_env != OTTO_ACP_TOKEN ]]; then
      local named=${!token_env:-}
      if [[ -z $named || $named != "${OTTO_ACP_TOKEN:-}" ]]; then
        die "connect.toml names token_env = \"$token_env\" but otto serve reads OTTO_ACP_TOKEN; name OTTO_ACP_TOKEN there, or export $token_env with the same value"
      fi
    fi
    if [[ -z $workspace_cfg || $workspace_cfg != /* ]]; then
      die "connect.toml [agent].workspace must be an absolute path, got '${workspace_cfg:-}'"
    fi
  fi
  workspace=${OTTO_CWD:-${workspace_cfg:-}}
  resolve_workspace
  if [[ -n ${OTTO_CWD:-} && -n $workspace_cfg && $(canonical_dir "$OTTO_CWD") != "$(canonical_dir "$workspace_cfg")" ]]; then
    warn "OTTO_CWD '$OTTO_CWD' differs from connect.toml workspace '$workspace_cfg'; serve must admit that workspace or connect's sessions will fail"
  fi
  if [[ -n ${OTTO_UI:-} || -n ${OTTO_EXTRA_ARGS:-} ]]; then
    warn "OTTO_UI and OTTO_EXTRA_ARGS are not used here, because otto serve rejects --ui and --prompt; use OTTO_SERVE_EXTRA_ARGS"
  fi
}

want_arg() { [[ -n ${2:-} ]] && serve_cmd+=("$1" "$2"); return 0; }

# build_serve_args fills serve_cmd from the shared otto.env settings. The ACP
# address and the workspace are written last so they win over
# OTTO_SERVE_EXTRA_ARGS: the address is what connect's configuration was
# checked against.
build_serve_args() {
  serve_cmd=(serve)
  want_arg --provider "${OTTO_PROVIDER:-}"
  want_arg --model "${OTTO_MODEL:-}"
  want_arg --profile "${OTTO_PROFILE:-}"
  want_arg --base-url "${OTTO_BASE_URL:-}"
  want_arg --sandbox "${OTTO_SANDBOX:-}"
  want_arg --thinking "${OTTO_THINKING:-}"
  want_arg --shell-timeout "${OTTO_SHELL_TIMEOUT:-}"
  want_arg --max-output-bytes "${OTTO_MAX_OUTPUT_BYTES:-}"
  if [[ -n ${OTTO_SERVE_EXTRA_ARGS:-} ]]; then
    local extra=()
    read -r -a extra <<<"${OTTO_SERVE_EXTRA_ARGS}"
    if [[ ${#extra[@]} -gt 0 ]]; then serve_cmd+=("${extra[@]}"); fi
  fi
  serve_cmd+=(--cwd "$workspace" --acp-listen "$acp_listen")
  if [[ -n ${OTTO_LISTEN:-} ]]; then serve_cmd+=(--listen "$OTTO_LISTEN"); fi
}

# ---- the supervisor ----

serve_pid=
connect_pid=
serve_started=0
connect_started=0
serve_offset=0
serve_ready=0
serve_fails=0
connect_fails=0
serve_next=0
connect_next=0
stop_requested=
restart_serve_requested=
restart_connect_requested=
exit_status=0

start_serve() {
  serve_offset=$(file_size "$serve_log")
  serve_started=$(now)
  serve_ready=0
  rm -f "$serve_pid_file"
  "$otto_bin" "${serve_cmd[@]}" >>"$serve_log" 2>&1 &
  serve_pid=$!
  printf '%s\n' "$serve_pid" >"$serve_pid_file"
  log "serve started pid=$serve_pid ${serve_cmd[*]}"
}

start_connect() {
  connect_started=$(now)
  rm -f "$connect_pid_file"
  "$connect_bin" --config "$connect_config" >>"$connect_log" 2>&1 &
  connect_pid=$!
  printf '%s\n' "$connect_pid" >"$connect_pid_file"
  log "connect started pid=$connect_pid --config $connect_config"
}

backoff() {
  local fails=$1 delay=$min_delay i=1
  while [[ $i -lt $fails ]]; do
    delay=$(( delay * 2 ))
    i=$(( i + 1 ))
  done
  [[ $delay -gt $max_delay ]] && delay=$max_delay
  printf '%s' "$delay"
}

# reap NAME PID UPTIME: waits for the child, logs its exit, and either
# schedules an immediate restart or backs off. Five consecutive fast failures
# stop the stack, so a broken configuration cannot loop forever.
reap() {
  local name=$1 pid=$2 uptime=$3 status=0 fails delay log_file
  wait "$pid" 2>/dev/null || status=$?
  if [[ $name == serve ]]; then
    serve_pid=
    serve_ready=0
    rm -f "$serve_pid_file"
    log_file=$serve_log
  else
    connect_pid=
    rm -f "$connect_pid_file"
    log_file=$connect_log
  fi
  log "$name exited pid=$pid status=$status uptime=${uptime}s"
  if [[ $uptime -ge $stable_after ]]; then
    if [[ $name == serve ]]; then
      serve_fails=0
      serve_next=$(now)
    else
      connect_fails=0
      connect_next=$(now)
    fi
    log "$name restarting immediately; it had run for ${uptime}s"
    return 0
  fi
  if [[ $name == serve ]]; then
    serve_fails=$(( serve_fails + 1 ))
    fails=$serve_fails
  else
    connect_fails=$(( connect_fails + 1 ))
    fails=$connect_fails
  fi
  if [[ $fails -ge $max_fails ]]; then
    log "$name failed $fails times in a row; stopping the stack; see $log_file"
    stop_requested=1
    exit_status=1
    return 0
  fi
  delay=$(backoff "$fails")
  if [[ $name == serve ]]; then
    serve_next=$(( $(now) + delay ))
  else
    connect_next=$(( $(now) + delay ))
  fi
  log "$name restarting in ${delay}s (fast failure $fails of $max_fails)"
}

supervise_serve() {
  if [[ -n $serve_pid ]] && ! alive "$serve_pid"; then
    reap serve "$serve_pid" "$(( $(now) - serve_started ))"
    return 0
  fi
  if [[ -z $serve_pid ]]; then
    if [[ $(now) -ge $serve_next ]]; then start_serve; fi
  elif [[ $serve_ready -eq 0 ]]; then
    if new_since "$serve_log" "$serve_offset" 'otto serve: ACP'; then
      serve_ready=1
      log "serve ACP listener ready on $acp_listen"
    fi
  fi
}

supervise_connect() {
  if [[ -n $connect_pid ]] && ! alive "$connect_pid"; then
    reap connect "$connect_pid" "$(( $(now) - connect_started ))"
    return 0
  fi
  [[ -n $connect_pid ]] && return 0
  [[ -n $serve_pid ]] || return 0
  [[ $(now) -ge $connect_next ]] || return 0
  # Connect reconnects on its next message, but starting it after serve
  # announced its listener keeps the first message from failing. The timeout is
  # a fallback for a serve that never prints that line.
  if [[ $serve_ready -eq 0 && $(( $(now) - serve_started )) -lt $ready_timeout ]]; then
    return 0
  fi
  start_connect
}

stop_child() {
  local name=$1 pid=$2 waited=0 limit=$(( stop_timeout * 5 ))
  [[ -n $pid ]] || return 0
  alive "$pid" || return 0
  log "stopping $name pid=$pid"
  kill -TERM "$pid" 2>/dev/null || true
  while alive "$pid" && [[ $waited -lt $limit ]]; do
    sleep 0.2
    waited=$(( waited + 1 ))
  done
  if alive "$pid"; then
    log "$name pid=$pid did not stop within ${stop_timeout}s; killing"
    kill -KILL "$pid" 2>/dev/null || true
  fi
  return 0
}

shutdown() {
  # Connect first: it cancels its running prompts and closes the connection,
  # and serve keeps the sessions the chat will load again.
  stop_child connect "$connect_pid"
  connect_pid=
  stop_child serve "$serve_pid"
  serve_pid=
  rm -f "$supervisor_pid_file" "$serve_pid_file" "$connect_pid_file"
  log "supervisor stopped"
  exit "$exit_status"
}

rotate_log() {
  local file=$1
  if [[ -s $file ]]; then mv -f "$file" "$file.1"; fi
}

run_supervisor() {
  local existing
  preflight
  build_serve_args
  if existing=$(live_pid_of "$supervisor_pid_file" "$PROGRAM"); then
    die "already running (pid $existing); use restart or stop"
  fi
  mkdir -p "$state_dir"
  rm -f "$supervisor_pid_file" "$serve_pid_file" "$connect_pid_file"
  # The supervisor's own stderr is rotated by `start` before it opens the log,
  # never here: this process already writes to that file.
  rotate_log "$serve_log"
  rotate_log "$connect_log"
  printf '%s\n' "$$" >"$supervisor_pid_file"
  trap 'stop_requested=1' TERM INT
  trap 'restart_serve_requested=1' USR1
  trap 'restart_connect_requested=1' USR2
  log "supervisor started pid=$$ acp=$acp_listen workspace=$workspace"
  serve_next=$(now)
  while :; do
    [[ -n $stop_requested ]] && break
    if [[ -n $restart_serve_requested ]]; then
      restart_serve_requested=
      log "restart requested for serve"
      stop_child serve "$serve_pid"
      serve_pid=
      serve_fails=0
      serve_next=$(now)
    fi
    if [[ -n $restart_connect_requested ]]; then
      restart_connect_requested=
      log "restart requested for connect"
      stop_child connect "$connect_pid"
      connect_pid=
      connect_fails=0
      connect_next=$(now)
    fi
    supervise_serve
    supervise_connect
    sleep "$poll"
  done
  shutdown
}

# ---- commands ----

start_stack() {
  local pid deadline
  preflight
  if pid=$(live_pid_of "$supervisor_pid_file" "$PROGRAM"); then
    die "already running (pid $pid); use restart or stop"
  fi
  mkdir -p "$state_dir"
  rm -f "$supervisor_pid_file" "$serve_pid_file" "$connect_pid_file"
  rotate_log "$supervisor_log"
  nohup "$self" run >>"$supervisor_log" 2>&1 &
  deadline=$(( $(now) + ready_timeout ))
  local stale
  while :; do
    if pid=$(live_pid_of "$supervisor_pid_file" "$PROGRAM"); then break; fi
    if stale=$(read_pid "$supervisor_pid_file") && ! alive "$stale"; then
      printf '%s: the supervisor exited; %s:\n' "$PROGRAM" "$supervisor_log" >&2
      tail -n 20 "$supervisor_log" >&2 || true
      exit 1
    fi
    if [[ $(now) -ge $deadline ]]; then
      printf '%s: the supervisor did not start; %s:\n' "$PROGRAM" "$supervisor_log" >&2
      tail -n 20 "$supervisor_log" >&2 || true
      exit 1
    fi
    sleep 0.2
  done
  deadline=$(( $(now) + ready_timeout + 10 ))
  while :; do
    if ! alive "$pid"; then
      printf '%s: the supervisor exited; %s:\n' "$PROGRAM" "$supervisor_log" >&2
      tail -n 20 "$supervisor_log" >&2 || true
      exit 1
    fi
    if grep -q -F 'otto serve: ACP' "$serve_log" 2>/dev/null && live_pid_of "$connect_pid_file" "$connect_bin" >/dev/null; then
      break
    fi
    if [[ $(now) -ge $deadline ]]; then
      warn "the stack did not come up within ${ready_timeout}s; check $supervisor_log"
      break
    fi
    sleep 0.2
  done
  status_stack
}

stop_stack() {
  # The supervisor stops connect and then serve, each within stop_timeout, so
  # this waits for both plus a margin.
  local pid waited=0 margin=$(( stop_timeout * 2 + 10 )) name file child
  local limit=$(( margin * 5 ))
  if pid=$(live_pid_of "$supervisor_pid_file" "$PROGRAM"); then
    log "stopping supervisor pid=$pid"
    kill -TERM "$pid" 2>/dev/null || true
    while alive "$pid" && [[ $waited -lt $limit ]]; do
      sleep 0.2
      waited=$(( waited + 1 ))
    done
    if alive "$pid"; then
      warn "supervisor pid=$pid did not stop within ${margin}s; killing"
      kill -KILL "$pid" 2>/dev/null || true
    fi
  else
    printf '%s: not running\n' "$PROGRAM"
  fi
  # A supervisor killed outright leaves its children behind.
  for name in connect serve; do
    eval "file=\${${name}_pid_file}"
    if child=$(live_pid_of "$file" ""); then
      warn "$name pid=$child was left behind; stopping it"
      kill -TERM "$child" 2>/dev/null || true
    fi
    rm -f "$file"
  done
  rm -f "$supervisor_pid_file"
}

signal_supervisor() {
  local signal=$1 name=$2 pid
  if ! pid=$(live_pid_of "$supervisor_pid_file" "$PROGRAM"); then
    die "not running; use start"
  fi
  log "asking supervisor pid=$pid to restart $name"
  kill -"$signal" "$pid"
}

restart_stack() {
  case ${1:-} in
  '' | all)
    stop_stack
    start_stack
    ;;
  serve) signal_supervisor USR1 serve ;;
  connect) signal_supervisor USR2 connect ;;
  *) die "restart takes serve, connect or nothing (the whole stack)" 2 ;;
  esac
}

health() {
  local addr
  addr=$(toml_table_key metrics listen "$connect_config")
  [[ -n $addr ]] || return 0
  command -v curl >/dev/null 2>&1 || return 0
  if curl -sf -m 2 -o /dev/null "http://$addr/healthz"; then printf ', healthz ok'; fi
}

status_stack() {
  local pid child
  if pid=$(live_pid_of "$supervisor_pid_file" "$PROGRAM"); then
    printf 'supervisor:  running (pid %s, up %s)\n' "$pid" "$(uptime_of "$pid")"
  else
    printf 'supervisor:  not running\n'
  fi
  if child=$(live_pid_of "$serve_pid_file" ""); then
    printf 'otto serve:  running (pid %s, up %s, ACP %s)\n' "$child" "$(uptime_of "$child")" "$acp_listen"
  else
    printf 'otto serve:  not running\n'
  fi
  if child=$(live_pid_of "$connect_pid_file" ""); then
    printf 'connect:     running (pid %s, up %s%s)\n' "$child" "$(uptime_of "$child")" "$(health)"
  else
    printf 'connect:     not running\n'
  fi
  printf 'state:       %s\n' "$state_dir"
}

logs_stack() {
  local file
  case ${1:-supervisor} in
  supervisor) file=$supervisor_log ;;
  serve) file=$serve_log ;;
  connect) file=$connect_log ;;
  *) die "logs takes supervisor, serve or connect" 2 ;;
  esac
  [[ -f $file ]] || die "$file does not exist yet"
  tail -n 80 -f "$file"
}

case ${1:-} in
start)
  shift
  [[ $# -eq 0 ]] || die "start takes no arguments" 2
  start_stack
  ;;
stop)
  shift
  [[ $# -eq 0 ]] || die "stop takes no arguments" 2
  stop_stack
  ;;
restart)
  shift
  [[ $# -le 1 ]] || die "restart takes serve, connect or nothing" 2
  restart_stack "${1:-}"
  ;;
status)
  shift
  [[ $# -eq 0 ]] || die "status takes no arguments" 2
  status_stack
  ;;
logs)
  shift
  [[ $# -le 1 ]] || die "logs takes supervisor, serve or connect" 2
  logs_stack "${1:-}"
  ;;
run)
  shift
  [[ $# -eq 0 ]] || die "run takes no arguments" 2
  run_supervisor
  ;;
help | -h | --help) usage ;;
'')
  usage >&2
  exit 2
  ;;
*)
  printf '%s: unknown command %s\n' "$PROGRAM" "$1" >&2
  usage >&2
  exit 2
  ;;
esac
