#!/bin/sh
# v0.6.9 (I4): lo SHA si legge dal tag, non da prima. Due volte in quattro
# fasi (v0.6.5 e v0.6.8) il REPORT dichiarò lo SHA di una build fatta
# PRIMA di un commit che toccava i sorgenti (clippy, fmt) — e l'aggancio
# di Homun si basa su quello SHA. La regola adesso sta nella macchina,
# non nella disciplina: questo script rifiuta un albero sporco, costruisce
# sotto nice (il Mac serve all'utente), e stampa il commit su cui ha
# costruito con dimensione e SHA256 dei tre file dell'aggancio.
#
# Uso:  scripts/build-sha.sh          # costruisce e stampa commit + SHA
#
# L'ultimo passo di una fase è lanciarlo DOPO il tag, con il repo sul tag
# (il commit stampato è la prova di dove ha costruito), e riportare nel
# REPORT quello che stampa: se i numeri differiscono da una build
# precedente, vale questa.

set -e

cd "$(dirname "$0")/.." || exit 1

# un albero sporco costruisce un binario che non appartiene a nessun
# commit: lo SHA non avrebbe significato
if [ -n "$(git status --porcelain)" ]; then
  echo "l'albero è sporco: committa o ripristina prima di costruire" >&2
  git status --porcelain >&2
  exit 1
fi

COMMIT=$(git rev-parse HEAD)
echo "costruzione sul commit $COMMIT"
echo "(tag: $(git describe --tags --exact-match 2>/dev/null || echo nessuno))"

cd crates/engramma-node || exit 1
# il Mac serve all'utente mentre si lavora: priorità minima
nice -n 19 npm run build

echo
echo "i tre file dell'aggancio, letti dal disco dopo la build:"
for f in engramma.darwin-arm64.node index.js index.d.ts; do
  if [ -f "$f" ]; then
    printf '  %-28s %10s B  SHA256 %s\n' "$f" \
      "$(stat -f%z "$f" 2>/dev/null || stat -c%s "$f")" \
      "$(shasum -a 256 "$f" | cut -d' ' -f1)"
  else
    echo "  MANCA $f" >&2
    exit 1
  fi
done
