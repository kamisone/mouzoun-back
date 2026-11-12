#!/usr/bin/env bash
# Keeps back/target small: removes this crate's own build output (API, binaries, the test
# program and every stale copy of them) and the incremental caches, keeping the compiled
# third-party libraries so the next build stays fast (~1 minute instead of a full rebuild).
#
#   scripts/prune-build.sh            prune now
#   scripts/prune-build.sh --hook     read a Claude Code PreToolUse payload on stdin and prune
#                                     only before a cargo build/test/clippy/check/run in back/
set -euo pipefail

BACK="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CARGO="${CARGO:-$HOME/.cargo/bin/cargo}"

prune() {
  [ -d "$BACK/target" ] || return 0
  (cd "$BACK" && "$CARGO" clean -p mouzoun-api --quiet) || true
  rm -rf "$BACK/target/debug/incremental" "$BACK/target/release/incremental"
}

if [ "${1:-}" = "--hook" ]; then
  payload="$(cat)"
  command="$(printf '%s' "$payload" | jq -r '.tool_input.command // ""')"
  cwd="$(printf '%s' "$payload" | jq -r '.cwd // ""')"
  # Only cargo commands that compile, run in (or pointed at) this back-end.
  printf '%s' "$command" | grep -Eq '(^|[;&|[:space:]])cargo[[:space:]]+(build|test|clippy|check|run)([[:space:]]|$)' || exit 0
  case "$cwd/ $command" in
    *"$BACK"*) prune ;;
  esac
  exit 0
fi

prune
