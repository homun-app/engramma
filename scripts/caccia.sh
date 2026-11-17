#!/bin/sh
# v0.6.6 (F2): la caccia lunga, un comando solo — così la rifà il revisore
# con lo stesso comando.
#
# Uso:  scripts/caccia.sh                  # ogni proprietà dell'inventario
#                                             a 20 000 casi, poi cinque
#                                             corse della suite del core a
#                                             2048 casi
#       scripts/caccia.sh 5000 3           # 5 000 casi per proprietà,
#                                             tre corse della suite
#
# Le proprietà vengono da crates/engramma-core/tests/properties.txt
# (l'inventario guardato da `properties_inventory`), mai da un elenco
# scritto qui a mano: una proprietà aggiunta alla suite entra nella caccia
# da sola, una tolta fa fallire l'inventario prima che la caccia parta.
# Tutto gira in release, a priorità minima (nice 19, due job di cargo, due
# fili di test) e, su macOS, sotto caffeinate -i: la finestra resta
# tranquilla per chi ci lavora.

set -e

CASI=${1:-20000}
CORSE=${2:-5}

# su macOS la corsa dura ore: il sonno del Mac non la interrompe
if [ "$(uname)" = Darwin ] && command -v caffeinate >/dev/null 2>&1 \
   && [ -z "${CACCIANO_CAFFE:-}" ]; then
  exec caffeinate -i env CACCIANO_CAFFE=1 "$0" "$@"
fi

export CARGO_BUILD_JOBS=2
export RUST_TEST_THREADS=2

INVENTARIO=crates/engramma-core/tests/properties.txt
[ -f "$INVENTARIO" ] || { echo "manca $INVENTARIO" >&2; exit 1; }

# l'inventario dev'essere onesto PRIMA di sprecare ore: il cancello di F2
cd "$(dirname "$0")/.."
nice -n 19 cargo test --release -p engramma-core --test properties_inventory \
  >/dev/null

adesso() { date +%s; }
inizio=$(adesso)

# una riga per proprietà: casi, secondi, esito
echo "caccia: $(grep -cve '^#' -e '^$' "$INVENTARIO") proprietà a $CASI casi, poi $CORSE corse della suite a 2048"
while read -r file nome; do
  [ -n "$file" ] || continue
  case $file in \#*) continue ;; esac
  pacchetto=$(basename "$(dirname "$(dirname "$file")")")
  bersaglio=$(basename "$file" .rs)
  printf '%s %s' "$nome" ''
  t0=$(adesso)
  if PROPTEST_CASES=$CASI nice -n 19 cargo test --release \
       -p "$pacchetto" --test "$bersaglio" "$nome" >/dev/null 2>&1; then
    esito=verde
  else
    esito=ROSSO
  fi
  echo "$esito $(( $(adesso) - t0 ))s ($pacchetto/$bersaglio)"
  [ "$esito" = verde ]
done < "$INVENTARIO"

# le cinque corse della suite del core
i=1
while [ "$i" -le "$CORSE" ]; do
  printf 'suite-%s ' "$i"
  t0=$(adesso)
  if PROPTEST_CASES=2048 nice -n 19 cargo test --release \
       -p engramma-core >/dev/null 2>&1; then
    esito=verde
  else
    esito=ROSSO
  fi
  echo "$esito $(( $(adesso) - t0 ))s (engramma-core, 2048 casi)"
  [ "$esito" = verde ]
  i=$((i + 1))
done

echo "caccia finita: $(( $(adesso) - inizio ))s in tutto"
