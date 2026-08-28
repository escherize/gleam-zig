#!/usr/bin/env bash
# Run the homepage's own setup block in a clean container and see whether a
# reader following it ends up somewhere useful.
#
# This exists because no amount of checking numbers catches the failure it is
# built for. An audit found the block cloned two repositories where four are
# needed: the harness writes simplifile into every scratch project and the tour
# supplies 59 of the 119 corpus programs, so following the page produced a
# checkout where not one corpus program could resolve. The page cites that
# corpus as its headline receipt.
#
# The block is extracted from the page, never copied, so the test cannot drift
# away from what readers actually see.
#
# Usage:
#   docs/check-setup-block.sh            # clone and build only, ~5 minutes
#   docs/check-setup-block.sh --corpus   # also run the corpus, much slower
#
# Needs docker and network. Not suitable for gating every push; run it on a
# schedule or when the docs change.

set -euo pipefail

DOCS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
IMAGE="${SETUP_CHECK_IMAGE:-ubuntu:24.04}"
RUN_CORPUS=0
[[ "${1:-}" == "--corpus" ]] && RUN_CORPUS=1

command -v docker >/dev/null || { echo "docker not found on PATH" >&2; exit 2; }

setup_block="$(python3 "$DOCS/extract-setup.py")" || {
  echo "could not extract the setup block from index.html" >&2
  exit 2
}

# The page states its own prerequisites. Install exactly those and nothing
# more: if the block needs something the page does not mention, this must fail.
prereqs="$(python3 - "$DOCS/index.html" <<'PY'
import re, sys, html
page = open(sys.argv[1], encoding="utf-8").read()
# Strip markup first: the sentence contains a link, so matching over raw HTML
# stops at the anchor rather than at the end of the list.
text = html.unescape(re.sub(r"<[^>]+>", "", page))
m = re.search(r"You need (.*?)(?:, and ~|\.)", text, re.S)
print(" ".join(m.group(1).split()) if m else "unknown")
PY
)"
echo "the page says a reader needs: ${prereqs}"
echo "image: ${IMAGE}"
echo

script="$(mktemp)"
trap 'rm -f "$script"' EXIT

{
  echo 'set -euo pipefail'
  echo 'export DEBIAN_FRONTEND=noninteractive'
  # Only the stated prerequisites, plus curl and git to obtain them.
  echo 'apt-get update -qq'
  echo 'apt-get install -y -qq git curl build-essential python3 nodejs >/dev/null'
  echo 'curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null'
  echo 'source "$HOME/.cargo/env"'
  echo 'echo "--- running the page.s setup block ---"'
  echo "$setup_block"
  echo 'echo "--- setup block finished, checking what a reader now has ---"'
  # The block ends inside the workspace directory.
  echo 'test -x gleam/target/debug/gleam || { echo "FAIL: no compiler binary"; exit 1; }'
  echo 'echo "compiler built: $(gleam/target/debug/gleam --version 2>&1 | head -1)"'
  echo 'for d in gleam gleam-stdlib simplifile tour; do'
  echo '  test -d "$d" || { echo "FAIL: the harness needs $d and the block did not clone it"; exit 1; }'
  echo 'done'
  echo 'echo "all four sibling checkouts present"'
  if [[ $RUN_CORPUS -eq 1 ]]; then
    echo 'echo "--- running the corpus the page cites as its receipt ---"'
    echo 'python3 examples/harness.py examples/rosetta/ | tail -3'
  fi
} > "$script"

echo "starting container, this takes a few minutes"
docker run --rm -i "$IMAGE" bash -s < "$script"
