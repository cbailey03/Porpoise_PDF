#!/usr/bin/env bash
#
# Runs every check from CONTRIBUTING.md in one pass.
#
# There is no CI, so this script is the checklist. It keeps going after a failure
# and prints a summary at the end, so one run tells you everything that is wrong
# instead of only the first thing.
#
# The test step sets PORPOISE_E2E=1, so the windowed end-to-end suite really runs.
# That opens real windows and needs a display.

set -u

cd "$(dirname "$0")/.." || exit 1

if [ -t 1 ]; then
  bold=$'\033[1m'; red=$'\033[31m'; green=$'\033[32m'; off=$'\033[0m'
else
  bold=''; red=''; green=''; off=''
fi

# Newline-separated rather than an array: macOS still ships bash 3.2, where an
# empty array under `set -u` is an error.
failures=''

# Runs one check, records a failure, and never aborts the run.
check() {
  name=$1
  shift
  printf '\n%s== %s ==%s\n' "$bold" "$name" "$off"
  start=$SECONDS
  if "$@"; then
    printf '%sok%s      %s (%ss)\n' "$green" "$off" "$name" "$((SECONDS - start))"
  else
    printf '%sFAILED%s  %s (%ss)\n' "$red" "$off" "$name" "$((SECONDS - start))"
    failures="$failures$name"$'\n'
  fi
}

# A check that cannot run is a failure, not a pass. A check that reports success
# without running is the exact bug this project already fixed once, for the
# end-to-end tests.
unavailable() {
  name=$1
  hint=$2
  printf '\n%s== %s ==%s\n' "$bold" "$name" "$off"
  printf '%sFAILED%s  %s: tool not installed. Get it with:\n    %s\n' "$red" "$off" "$name" "$hint"
  failures="$failures$name"$'\n'
}

# Guards this project's central claim: no C PDF or codec library in the shipped
# binary. Inverted on purpose, since a grep hit is the failure. `cargo tree` is
# run on its own so that a broken tree cannot pass as "nothing matched".
no_c_codecs() {
  tree=$(cargo tree --package porpoise-app --edges normal) || return 1
  hits=$(printf '%s\n' "$tree" | grep -iE 'pdfium|mupdf|openjpeg|jpeg2k|jbig2dec|testkit') || return 0
  printf 'reached by a normal dependency edge:\n%s\n' "$hits"
  return 1
}

check "fmt"     cargo fmt --all --check
check "clippy"  cargo clippy --workspace --all-targets --all-features -- -D warnings
check "tests"   env PORPOISE_E2E=1 cargo test --workspace --all-features

if command -v cargo-deny >/dev/null 2>&1; then
  check "deny"  cargo deny check bans licenses sources advisories
else
  unavailable "deny" "cargo install --locked cargo-deny"
fi

if rustup toolchain list 2>/dev/null | grep -q '^1\.92'; then
  check "msrv"  cargo +1.92 check --workspace --all-features --all-targets
else
  unavailable "msrv" "rustup toolchain install 1.92"
fi

check "codecs"  no_c_codecs

printf '\n%s== summary ==%s\n' "$bold" "$off"
if [ -z "$failures" ]; then
  printf '%sall checks passed%s (%ss)\n' "$green" "$off" "$SECONDS"
  exit 0
fi
printf '%sfailed:%s\n' "$red" "$off"
printf '%s' "$failures" | sed 's/^/  /'
exit 1
