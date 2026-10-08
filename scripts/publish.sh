#!/usr/bin/env bash
# Publish every workspace crate to crates.io in dependency order.
#
# Handles the two known gotchas for a fresh-workspace `cargo publish`:
#   1. Verification catch-22: a non-leaf crate's verification build fails
#      until its dependencies have propagated to the sparse index. Solved by
#      polling `cargo info --registry crates-io` between publishes.
#   2. crates.io rate limit for first-time crates: burst of 5, then 1 per
#      10 minutes. Solved by throttling after the burst window.
#
# Idempotent: crates already on crates.io are skipped, so you can safely
# re-run the script after any failure.
#
# Usage:
#   scripts/publish.sh                      # burst-aware, default
#   scripts/publish.sh --no-burst           # throttle every publish (safest
#                                           # after a rate-limit rejection)
#   scripts/publish.sh --dry-run            # log what would happen, do nothing
#
# Requires:
#   - cargo logged in (`cargo login <token>`) with publish permissions for
#     the `epistates` crates.io namespace
#   - a working directory the script is comfortable making dirty (passes
#     `--allow-dirty` to cargo publish)
#   - jq

set -euo pipefail

# --- configuration ----------------------------------------------------------

BURST_SIZE=5
THROTTLE_SECONDS=610          # 10 min + safety margin
INDEX_POLL_INTERVAL=5
INDEX_POLL_TIMEOUT=300        # give up after 5 minutes of waiting for the index
POST_BURST_SETTLE=8           # short pause between burst-window publishes

NO_BURST=0
DRY_RUN=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --no-burst) NO_BURST=1; shift ;;
    --dry-run)  DRY_RUN=1;  shift ;;
    -h|--help)
      sed -n '2,25p' "$0"
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

# Move to workspace root (parent of scripts/)
cd "$(dirname "$0")/.."

WORKSPACE_VERSION="$(
  awk '
    /^\[workspace\.package\]$/ { in_workspace_package = 1; next }
    /^\[/ { in_workspace_package = 0 }
    in_workspace_package && $1 == "version" {
      gsub(/"/, "", $3)
      print $3
      exit
    }
  ' Cargo.toml
)"

if [[ -z "$WORKSPACE_VERSION" ]]; then
  echo "failed to read workspace package version from Cargo.toml" >&2
  exit 1
fi

# Publishable crates in dependency order (each depends only on crates
# earlier in the list), derived from the manifests so it cannot drift.
CRATES=()
while IFS= read -r crate; do CRATES+=("$crate"); done < <(
  cargo metadata --format-version 1 --no-deps | jq -r '
    [.packages[] | select(.publish != [])] as $pkgs
    | ($pkgs | map(.name)) as $names
    | ($pkgs
       | map({key: .name,
              value: ([.dependencies[] | select(.kind != "dev") | .name | select(IN($names[]))] | unique)})
       | from_entries) as $deps
    | def visit($order):
        ($names | map(select(. as $n | ($order | index($n)) == null and ($deps[$n] - $order | length) == 0)) | first) as $next
        | if $next == null then $order else visit($order + [$next]) end;
      visit([]) | .[]
  '
)
EXPECTED="$(cargo metadata --format-version 1 --no-deps | jq '[.packages[] | select(.publish != [])] | length')"
if [[ ${#CRATES[@]} -ne $EXPECTED ]]; then
  echo "dependency cycle among workspace crates: ordered ${#CRATES[@]} of $EXPECTED" >&2
  exit 1
fi

# --- helpers ----------------------------------------------------------------

log()  { printf '[%s] %s\n' "$(date +%H:%M:%S)" "$*"; }
step() { printf '\n[%s] === %s ===\n' "$(date +%H:%M:%S)" "$*"; }

already_published() {
  local crate="$1"
  # `cargo info --registry crates-io` prints `version: x.y.z` for the latest
  # published version. Anything else (error, empty) means not published.
  cargo info "$crate" --registry crates-io 2>/dev/null \
    | grep -Fxq "version: $WORKSPACE_VERSION"
}

wait_for_index() {
  local crate="$1"
  local waited=0
  while (( waited < INDEX_POLL_TIMEOUT )); do
    if already_published "$crate"; then
      log "  [index] $crate is live"
      return 0
    fi
    sleep "$INDEX_POLL_INTERVAL"
    waited=$(( waited + INDEX_POLL_INTERVAL ))
    log "  [index] still waiting for $crate in index (${waited}s)"
  done
  log "  [index] TIMEOUT after ${INDEX_POLL_TIMEOUT}s"
  return 1
}

publish_one() {
  local crate="$1"
  step "Publishing $crate"
  if (( DRY_RUN )); then
    log "  [dry-run] would: cargo publish -p $crate --allow-dirty"
    return 0
  fi
  if ! cargo publish -p "$crate" --allow-dirty; then
    log "  FAILED. Re-run this script to resume (already-published crates are skipped)."
    exit 1
  fi
  wait_for_index "$crate" || {
    log "  WARNING: $crate uploaded but index did not reflect it within ${INDEX_POLL_TIMEOUT}s."
    log "  The next crate's verification may fail. Re-run this script to retry."
    exit 1
  }
}

# --- main -------------------------------------------------------------------

if (( DRY_RUN == 0 )); then
  step "Cleaning target/package/"
  rm -rf target/package
fi

published_this_run=0

for i in "${!CRATES[@]}"; do
  crate="${CRATES[$i]}"

  if already_published "$crate"; then
    log "[skip] $crate already on crates.io"
    continue
  fi

  # Throttle before publishing (not after) so resume semantics are clean.
  if (( DRY_RUN == 0 && published_this_run > 0 )); then
    if (( NO_BURST )) || (( published_this_run >= BURST_SIZE )); then
      log "Sleeping ${THROTTLE_SECONDS}s to respect the 1-per-10-minute rate limit..."
      sleep "$THROTTLE_SECONDS"
    else
      log "Burst window: short ${POST_BURST_SETTLE}s settle pause"
      sleep "$POST_BURST_SETTLE"
    fi
  fi

  publish_one "$crate"
  published_this_run=$(( published_this_run + 1 ))
done

step "All ${#CRATES[@]} crates are on crates.io"
