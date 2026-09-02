#!/usr/bin/env bash
#
# Stop MDM and everything it started.
#
# The order is the whole point of this script:
#
#   the browser  starts the native messaging host on demand, and restarts it
#                half a second after it dies (extension/src/native.js)
#   mdm-host     launches the app whenever it cannot reach the app's socket
#   mdm          starts aria2c and tells it to exit when this pid does
#   aria2c       outlives the app briefly, and holds the RPC port until it goes
#
# Killing any of them alone puts it straight back, so they are stopped from the
# top of that chain down. The browser is the one link this script will not cut
# without being asked — see --quit-browser.
set -euo pipefail

CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/mdm"
DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/mdm"
SESSION="$DATA_DIR/aria2.session"
# Mirrors paths::runtime_dir(): XDG_RUNTIME_DIR when the session has one, and a
# uid-suffixed directory under TMPDIR when it does not.
if [[ -n "${XDG_RUNTIME_DIR:-}" ]]; then
  SOCKET="$XDG_RUNTIME_DIR/mdm/mdm.sock"
else
  SOCKET="${TMPDIR:-/tmp}/mdm-$(id -u)/mdm.sock"
fi

QUIET=
FORCE=
QUIT_BROWSER=

say()  { [[ -n "$QUIET" ]] || printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m warning:\033[0m %s\n' "$*"; }
die()  { printf '\033[1;31m error:\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'USAGE'
Usage: stop.sh [options]

Stops the MDM app, the browser's native messaging host that restarts it, and
the aria2c daemon they share — in an order that makes them stay stopped.

  -b, --quit-browser  also close the browser that keeps restarting MDM. Without
                      this, MDM comes back about half a second after it is
                      stopped, for as long as the add-on is enabled.
  -f, --force         skip the graceful stop and SIGKILL everything at once
  -q, --quiet         print only warnings and errors
  -h, --help          this text

Exits non-zero if anything is still running afterwards, so it can gate an
install:  ./stop.sh && ./install.sh
USAGE
}

while (( $# )); do
  case "$1" in
    -b|--quit-browser) QUIT_BROWSER=yes ;;
    -f|--force)        FORCE=yes ;;
    -q|--quiet)        QUIET=yes ;;
    -h|--help)         usage; exit 0 ;;
    *)                 die "unknown option: $1 (try --help)" ;;
  esac
  shift
done

# ---------------------------------------------------------------- processes

# `kill -0` and /proc both still see a zombie, so ask for the state instead: a
# process in Z has already exited and is only waiting to be collected.
alive() {
  local state
  state="$(ps -o stat= -p "$1" 2>/dev/null | tr -d ' ')"
  [[ -n "$state" && "$state" != Z* ]]
}

# Exact-name matches, split by whether there is anything left to kill. Both
# end on `return 0`: these are read through $(...) into an assignment, which
# under `set -e` would abort the script on a non-zero status.
live_pids()   { local p; for p in $(pgrep -x "$1" 2>/dev/null || true); do alive "$p" && echo "$p"; done; return 0; }
zombie_pids() { local p; for p in $(pgrep -x "$1" 2>/dev/null || true); do alive "$p" || echo "$p"; done; return 0; }

# Only the aria2c *we* started. Matching on the session path rather than the
# RPC port matters: the port is a setting the user can change, and killing a
# stranger's aria2c would take out a download that has nothing to do with MDM.
our_aria2c() {
  local pid cmdline
  for pid in $(pgrep -x aria2c 2>/dev/null || true); do
    cmdline="$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null || true)"
    [[ "$cmdline" == *"--save-session=$SESSION"* ]] && echo "$pid"
  done
  return 0
}

# Whatever owns a running host: the browser, and the only thing here that can
# bring MDM back on its own.
relaunchers() {
  local host ppid comm seen=" "
  for host in $(live_pids mdm-host); do
    ppid="$(ps -o ppid= -p "$host" 2>/dev/null | tr -d ' ')"
    [[ -n "$ppid" && "$ppid" != 1 ]] || continue
    [[ "$seen" == *" $ppid "* ]] && continue
    seen+="$ppid "
    comm="$(ps -o comm= -p "$ppid" 2>/dev/null | tr -d ' ')"
    [[ -n "$comm" ]] && echo "$ppid:$comm"
  done
  return 0
}

commas() { local s="$*"; echo "${s// /, }"; }

# SIGTERM, then SIGKILL for whatever is left after `grace` seconds.
stop_pids() { # stop_pids <label> <grace-seconds> <pid>...
  local label="$1" grace="$2"; shift 2
  (( $# )) || return 0

  if [[ -n "$FORCE" ]]; then
    say "  killing $label (pid $(commas "$@"))"
    kill -KILL "$@" 2>/dev/null || true
    return 0
  fi

  say "  stopping $label (pid $(commas "$@"))"
  kill -TERM "$@" 2>/dev/null || true

  local waited=0 remaining=() p
  while (( waited < grace * 10 )); do
    remaining=()
    for p in "$@"; do alive "$p" && remaining+=("$p"); done
    (( ${#remaining[@]} )) || return 0
    sleep 0.1
    waited=$(( waited + 1 ))
  done

  warn "$label did not exit within ${grace}s; sending SIGKILL"
  kill -KILL "${remaining[@]}" 2>/dev/null || true
  sleep 0.2
  return 0
}

# ---------------------------------------------------------------- survey

host_pids="$(live_pids mdm-host)"
app_pids="$(live_pids mdm)"
aria_pids="$(our_aria2c)"
dead_apps="$(zombie_pids mdm)"
# Remembered so the check at the end can tell "never died" from "came back".
was_running=" $host_pids $app_pids $aria_pids "

if [[ -z "${host_pids}${app_pids}${aria_pids}${dead_apps}" ]]; then
  say "MDM is not running"
  exit 0
fi

# ---------------------------------------------------------------- browser

browsers="$(relaunchers)"

# Ask the browsers among them to quit, and report the rest. Returns non-zero
# when anything that can relaunch MDM is still up afterwards.
quit_browsers() {
  local entry pids=() still=() p
  for entry in $browsers; do
    # Only ever signal something recognisable as a browser: the parent of a
    # host started by hand from a terminal is that terminal's shell, and
    # --quit-browser must not take that out along with the session it is in.
    case "${entry#*:}" in
      firefox*|librewolf*|waterfox*|floorp*|zen*|iceweasel*|*-browser)
        pids+=("${entry%%:*}") ;;
      *)
        warn "the host was started by ${entry#*:} (pid ${entry%%:*}), which is not a
  browser this script will close. Stop it yourself if MDM keeps coming back."
        return 1 ;;
    esac
  done
  (( ${#pids[@]} )) || return 1

  # SIGTERM only, and never SIGKILL: this is a clean browser shutdown, which is
  # what writes the session back out. Losing someone's open tabs to save a few
  # seconds is not a trade this script gets to make.
  say "  asking $(commas "${pids[@]}") to quit"
  kill -TERM "${pids[@]}" 2>/dev/null || true
  for _ in {1..300}; do
    still=()
    for p in "${pids[@]}"; do alive "$p" && still+=("$p"); done
    (( ${#still[@]} )) || return 0
    sleep 0.1
  done
  die "the browser (pid $(commas "${still[@]}")) has not quit after 30s.
  Close it yourself and re-run — killing it outright would lose the session."
}

# The extension reconnects 500 ms after its host dies and says hello, and that
# hello is what makes the new host launch the app. So while the add-on is
# enabled nothing below can keep MDM down. Say so before killing anything,
# rather than reporting it as a failure afterwards.
if [[ -n "$browsers" ]]; then
  quit_it=
  if [[ -n "$QUIT_BROWSER" ]]; then
    say "Closing the browser that restarts MDM"
    quit_browsers && quit_it=yes
  fi
  if [[ -z "$quit_it" ]]; then
    names=""
    for entry in $browsers; do names+="${entry#*:} (pid ${entry%%:*}), "; done
    warn "${names%, } is running the MDM add-on and will start MDM again about half
  a second after it is stopped. Everything below still stops — it just will not
  stay stopped. Re-run with --quit-browser, or disable the add-on first."
  fi
fi

# ---------------------------------------------------------------- stop

say "Stopping MDM"

# The relauncher first: anything killed before it comes straight back.
# shellcheck disable=SC2086
stop_pids "the native messaging host" 3 $(live_pids mdm-host)

# A zombie cannot be signalled — it has already exited, and only its parent
# calling wait() clears it from the process table. mdm-host spawns the app and
# never waits on it, so the corpse stays visible to pgrep, to this script and
# to install.sh's "still running" guard for as long as the host lives. Killing
# the host hands the zombie to init, which reaps it at once.
if [[ -n "$dead_apps" ]]; then
  say "  reaping an already-dead app (pid $(commas $dead_apps))"
  for _ in {1..20}; do
    dead_apps="$(zombie_pids mdm)"
    [[ -n "$dead_apps" ]] || break
    sleep 0.1
  done
  for pid in $dead_apps; do
    parent="$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ' || true)"
    warn "pid $pid has exited but its parent has not collected it. Nothing can
  kill a process that is already dead — stop its parent instead:
    $(ps -o pid=,cmd= -p "${parent:-1}" 2>/dev/null || echo "pid ${parent:-unknown}")"
  done
fi

# shellcheck disable=SC2086
stop_pids "the app" 5 $(live_pids mdm)

# aria2c polls for the app's pid (--stop-with-process) and shuts itself down
# once it goes, saving its session as it exits. Let it: the app is killed here
# rather than closed, so the app's own "save and shut down" never runs, and
# this is the only chance the unfinished downloads get.
aria_pids="$(our_aria2c)"
if [[ -n "$aria_pids" && -z "$FORCE" ]]; then
  say "  waiting for aria2c to save its session and exit"
  for _ in {1..40}; do
    aria_pids="$(our_aria2c)"
    [[ -n "$aria_pids" ]] || break
    sleep 0.1
  done
fi
# shellcheck disable=SC2086
stop_pids "aria2c" 5 $aria_pids

# The app clears a stale socket itself on the next start, so this is tidiness
# rather than a requirement — it just keeps the runtime directory from
# suggesting MDM is up.
if [[ -S "$SOCKET" && -z "$(live_pids mdm)" ]]; then
  rm -f "$SOCKET"
fi

# ---------------------------------------------------------------- verify

# What actually blocks the next launch is the RPC port: the supervisor waits
# ten seconds for it and then refuses to start, so check the port rather than
# trust that the kills landed.
port="$(sed -n 's/^rpcPort *= *\([0-9]*\).*/\1/p' "$CONFIG_DIR/settings.toml" 2>/dev/null | head -1)"
port="${port:-6810}"

problems=()
relaunched=
note() { # note <name> <pids>
  local name="$1" pids="$2" p
  for p in $pids; do
    if [[ "$was_running" == *" $p "* ]]; then
      problems+=("$name (pid $p) did not stop")
    else
      problems+=("$name (pid $p) was started again while this script ran")
      relaunched=yes
    fi
  done
}
note mdm-host "$(live_pids mdm-host)"
note mdm      "$(live_pids mdm)"
note aria2c   "$(our_aria2c)"
[[ -n "$(zombie_pids mdm)" ]] && problems+=("a dead mdm (pid $(commas $(zombie_pids mdm))) has not been collected by its parent")
if command -v ss >/dev/null; then
  holder="$(ss -lntpH "sport = :$port" 2>/dev/null |
              sed -n 's/.*users:(("\([^"]*\)",pid=\([0-9]*\).*/\1 (pid \2)/p' | head -1)"
  [[ -n "$holder" ]] && problems+=("port $port is still held by $holder")
fi

if (( ${#problems[@]} )); then
  for p in "${problems[@]}"; do warn "$p"; done
  if [[ -n "$relaunched" ]]; then
    die "MDM was restarted while this script ran. The browser does that through
  the extension, and it will keep doing it — re-run with --quit-browser, or
  disable the add-on in about:addons first."
  fi
  die "MDM did not stop completely. Re-run with --force to SIGKILL what is left."
fi

say "Stopped"
