#!/bin/zsh
# v0.6.5 scratch: the whole remaining pipeline, resilient to a machine
# at its process limit. Every step retries every 5 minutes until done;
# everything lands in .tmp-e65-retry.log.
cd ~/Projects/Bot/engramma
export PATH="$HOME/.cargo/bin:$PATH"
export PROPTEST_CASES=${PROPTEST_CASES:-2048}
LOG=~/Projects/Bot/engramma/.tmp-e65-retry.log
try() {
  local name="$1"; shift
  local tries=0
  # skip steps already done (marker files)
  local marker=".tmp-done-$name"
  if [ -f "$marker" ]; then
    echo "=== $name already done ===" >> "$LOG"
    return 0
  fi
  while true; do
    tries=$((tries + 1))
    echo "=== $name (try $tries, $(date +%H:%M:%S)) ===" >> "$LOG"
    if "$@" >> "$LOG" 2>&1; then
      echo "=== $name OK ===" >> "$LOG"
      touch "$marker"
      return 0
    fi
    if [ $tries -ge 60 ]; then
      echo "=== $name GAVEUP after $tries ===" >> "$LOG"
      return 1
    fi
    /bin/sleep 300
  done
}

# 1) the suites
try protocol cargo test -p engramma-sync --release --test protocol_version
try core1 cargo test -p engramma-core --release
try cli cargo test -p engramma-cli --release
try sync cargo test -p engramma-sync --release
try nodecargo cargo test -p engramma-node --release
try mcp cargo test -p engramma-mcp --release

# 2) the measurements (--ignored)
try freq cargo test -p engramma-core --release --test convergence -- --ignored --nocapture generator_frequencies
try d5tables cargo test -p engramma-core --release --test conflict_cost -- --ignored --nocapture d5_the_copies
try d6fleet cargo test -p engramma-core --release --test conflict_cost -- --ignored --nocapture d6_ten_percent
try r3cost cargo test -p engramma-core --release --test diff3_linear -- --ignored --nocapture r3_cost
try e3bench cargo test -p engramma-core --release --test conflict_cost -- --ignored --nocapture rederivation

# 3) the node build + JS tests
try npmbuild sh -c 'cd crates/engramma-node && npm run build'
try npmtest sh -c 'cd crates/engramma-node && npm test'
try npmelectron sh -c 'cd crates/engramma-node && npm run test:electron'

# 4) Linux
try linux sh scripts/linux.sh

echo "=== PIPELINE DONE $(date) ===" >> "$LOG"
