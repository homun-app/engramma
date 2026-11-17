#!/bin/zsh
# v0.6.5 scratch: the final hunt — four properties at 20 000 cases each,
# then five core-suite runs at PROPTEST_CASES=2048. Resilient to the
# machine's process limit: every step retries until done.
cd ~/Projects/Bot/engramma
export PATH="$HOME/.cargo/bin:$PATH"
LOG=~/Projects/Bot/engramma/.tmp-e65-hunt.log
try() {
  local name="$1"; shift
  local marker=".tmp-done-$name"
  if [ -f "$marker" ]; then
    echo "=== $name already done ===" >> "$LOG"
    return 0
  fi
  local tries=0
  while true; do
    tries=$((tries + 1))
    echo "=== $name (try $tries, $(date +%H:%M:%S)) ===" >> "$LOG"
    if "$@" >> "$LOG" 2>&1; then
      echo "=== $name OK ===" >> "$LOG"
      touch "$marker"
      return 0
    fi
    if [ $tries -ge 30 ]; then
      echo "=== $name GAVEUP ===" >> "$LOG"
      return 1
    fi
    /bin/sleep 240
  done
}

try hunt-converges env PROPTEST_CASES=20000 cargo test -p engramma-core --release --test convergence the_fleet_converges -- --nocapture
try hunt-reconstruction env PROPTEST_CASES=20000 cargo test -p engramma-core --release --test convergence every_copy_equals_its_reconstruction -- --nocapture
try hunt-monotonicity env PROPTEST_CASES=20000 cargo test -p engramma-core --release --test convergence no_copy_closes_without_a_witness -- --nocapture
try hunt-notextlost env PROPTEST_CASES=20000 cargo test -p engramma-core --release --test convergence no_body_text_is_lost -- --nocapture
try core2048-a env PROPTEST_CASES=2048 cargo test -p engramma-core --release
try core2048-b env PROPTEST_CASES=2048 cargo test -p engramma-core --release
try core2048-c env PROPTEST_CASES=2048 cargo test -p engramma-core --release
try core2048-d env PROPTEST_CASES=2048 cargo test -p engramma-core --release
try core2048-e env PROPTEST_CASES=2048 cargo test -p engramma-core --release
echo "=== HUNT DONE $(date) ===" >> "$LOG"
