#!/bin/sh
# v0.4.4 (P2): Linux arm64 check of the whole workspace, in Docker, offline.
#
# Uso:  scripts/linux.sh            # fmt + clippy + test nel container
#       scripts/linux.sh build-node # in più: cargo build -p engramma-node --release
#
# L'immagine `engramma-linux:1.95.0` (Debian bookworm, Rust 1.95.0 aarch64,
# gcc 12, clippy e rustfmt) dev'essere già costruita. Il sorgente è montato
# in SOLA LETTURA (un test che scrive nel repo è un difetto), la rete è
# chiusa (`--network none`: un test che cerca la rete è un difetto), si
# compila come utente normale (root ignora i permessi dei file e i test che
# li usano mentirebbero). Il target sta nel volume `engramma-linux-target`,
# non nel repo: alla fine della fase, `docker volume rm
# engramma-linux-target`.

set -e

CMD='cargo fmt --check && cargo clippy --all-targets --offline -- -D warnings && cargo test --offline --workspace --no-fail-fast'
if [ "${1:-}" = "build-node" ]; then
  CMD="$CMD && cargo build -p engramma-node --release"
fi

docker volume create engramma-linux-target
# il volume nasce di root: una volta sola, lo si dà all'utente che compila
docker run --rm -v engramma-linux-target:/target engramma-linux:1.95.0 \
  chown -R "$(id -u):$(id -g)" /target
# v0.6.9: la CPU del container è limitata a due -- il Mac serve
# all'utente mentre la verifica gira (prima era disciplina di chi
# lanciava, ora è della macchina, come la caccia e lo SHA)
docker run --rm --cpus=2 --network none --user "$(id -u):$(id -g)" -e HOME=/tmp \
  -v "$PWD":/src:ro -v "$HOME/.cargo/registry":/usr/local/cargo/registry:ro \
  -v "$HOME/.cache/engramma":/tmp/.cache/engramma:ro \
  -v engramma-linux-target:/target -e CARGO_TARGET_DIR=/target -e CARGO_NET_OFFLINE=true \
  -w /src engramma-linux:1.95.0 \
  sh -c "$CMD"
