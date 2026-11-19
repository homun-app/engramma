#!/bin/sh
# v0.6.6 (F2): la caccia lunga, un comando solo — così la rifà il revisore
# con lo stesso comando.
#
# v0.6.9 (I3): la caccia non si ferma più a metà. Girava al primo rosso e
# nella v0.6.8 un difetto noto lasciò 22 proprietà su 26 senza giudizio —
# e il difetto I1 stava fra quelle. Ora gira SEMPRE tutte le proprietà
# dell'inventario e tutte le corse della suite: un rosso si scrive, si
# salva il seme, e si va avanti. Alla fine stampa il riepilogo ed esce
# con un codice diverso da zero se c'è almeno un rosso.
#
# I difetti APERTI stanno in crates/engramma-core/tests/open-defects.txt
# (proprietà, semi, riferimento al REPORT) e le loro proprietà si girano
# COMUNQUE: un difetto dichiarato non è un motivo per non giudicare il
# resto. Il giudizio su un rosso — «è il difetto dichiarato o uno nuovo?»
# con la riproduzione del seme — sta nel REPORT, scritto a mano. I semi
# dei difetti aperti devono stare FUORI dai file delle regressioni (la
# caccia lo controlla prima di partire: una suite che parte già rossa
# non misura niente; lo controlla anche il test
# open_defect_seeds_stay_out_of_the_regression_files).
#
# Uso:  scripts/caccia.sh                  # ogni proprietà dell'inventario
#                                             a 20 000 casi, poi cinque
#                                             corse della suite del core a
#                                             2048 casi
#       scripts/caccia.sh 5000 3           # 5 000 casi per proprietà,
#                                             tre corse della suite
#
# Le proprietà vengono da crates/engramma-core/tests/properties.txt
# (l'inventario guardato da `properties_inventory`, che ora guarda anche
# i difetti aperti), mai da un elenco scritto qui a mano: una proprietà
# aggiunta alla suite entra nella caccia da sola, una tolta fa fallire
# l'inventario prima che la caccia parta. Per ogni corsa: proprietà,
# casi, esito, tempo, e per i rossi il seme (il registro intero sta in
# target/caccia/<inizio>/). Tutto gira in release, a priorità minima
# (nice 19, due job di cargo, due fili di test) e, su macOS, sotto
# caffeinate -i: la finestra resta tranquilla per chi ci lavora.

CASi=${1:-20000}
CORSE=${2:-5}

# su macOS la corsa dura ore: il sonno del Mac non la interrompe
if [ "$(uname)" = Darwin ] && command -v caffeinate >/dev/null 2>&1 \
   && [ -z "${CACCIANO_CAFFE:-}" ]; then
  exec caffeinate -i env CACCIANO_CAFFE=1 "$0" "$@"
fi

export CARGO_BUILD_JOBS=2
export RUST_TEST_THREADS=2

INVENTARIO=crates/engramma-core/tests/properties.txt
APERTI=crates/engramma-core/tests/open-defects.txt
[ -f "$INVENTARIO" ] || { echo "manca $INVENTARIO" >&2; exit 1; }
[ -f "$APERTI" ] || { echo "manca $APERTI" >&2; exit 1; }

cd "$(dirname "$0")/.." || exit 1

# l'inventario dev'essere onesto PRIMA di sprecare ore: il cancello di F2
# (ora guarda anche che ogni difetto aperto nomini una proprietà vera e
# che i suoi semi stiano fuori dai file delle regressioni)
nice -n 19 cargo test --release -p engramma-core --test properties_inventory \
  >/dev/null || { echo "l'inventario o i difetti aperti non tornano" >&2; exit 1; }

# il registro di questa caccia: il primo rosso ha la sua traccia intera
INIZIO=$(date +%Y%m%d-%H%M%S)
LOGDIR=target/caccia/$INIZIO
mkdir -p "$LOGDIR"

adesso() { date +%s; }
inizio=$(adesso)
rossi=0

# una proprietà per riga: casi, secondi, esito, e il seme minimo se rossa.
# Il rosso NON ferma la caccia (v0.6.9, I3): si scrive e si continua.
echo "caccia: $(grep -cve '^#' -e '^$' "$INVENTARIO") proprietà a $CASi casi, poi $CORSE corse della suite a 2048"
echo "difetti aperti dichiarati (si girano comunque):"
sed -e 's/#.*//' -e '/^$/d' -e '/^#/d' "$APERTI" | while read -r file nome semi _x; do
  [ -n "$file" ] || continue
  echo "  - $nome: $semi"
done
echo "i semi dei difetti aperti stanno fuori dai file delle regressioni (controllato dall'inventario)"

while read -r file nome; do
  [ -n "$file" ] || continue
  case $file in \#*) continue ;; esac
  pacchetto=$(basename "$(dirname "$(dirname "$file")")")
  bersaglio=$(basename "$file" .rs)
  etichetta="$pacchetto/$nome"
  printf '%s %s' "$nome" ''
  t0=$(adesso)
  registro="$LOGDIR/prop-$nome.log"
  if PROPTEST_CASES=$CASi nice -n 19 cargo test --release \
       -p "$pacchetto" --test "$bersaglio" "$nome" >"$registro" 2>&1; then
    esito=verde
    printf 'verde %ss (%s)\n' "$(( $(adesso) - t0 ))" "$etichetta"
  else
    esito=ROSSO
    rossi=$((rossi + 1))
    # il seme che proptest stampa nella sua diagnosi (dopo lo shrinking
    # è il minimo), e la riga del file di regressione da NON scrivere
    semi=$(grep -Eo 'seed [=:] [0-9]+' "$registro" | head -3 | tr '\n' ' ')
    printf 'ROSSO %ss (%s) semi: %s\n' "$(( $(adesso) - t0 ))" "$etichetta" "${semi:-?}"
    echo "  registro: $registro"
  fi
done < "$INVENTARIO"

# le corse della suite del core (2048 casi): i semi dei difetti aperti
# stanno fuori dalle regressioni, quindi una caduta qui è sempre una
# notizia — dichiarata nel riepilogo, giudicata nel REPORT
i=1
while [ "$i" -le "$CORSE" ]; do
  printf 'suite-%s ' "$i"
  t0=$(adesso)
  registro="$LOGDIR/suite-$i.log"
  if PROPTEST_CASES=2048 nice -n 19 cargo test --release \
       -p engramma-core >"$registro" 2>&1; then
    esito=verde
    printf 'verde %ss (engramma-core, 2048 casi)\n' "$(( $(adesso) - t0 ))"
  else
    esito=ROSSO
    rossi=$((rossi + 1))
    semi=$(grep -Eo 'seed [=:] [0-9]+' "$registro" | head -3 | tr '\n' ' ')
    printf 'ROSSO %ss (engramma-core, 2048 casi) semi: %s\n' "$(( $(adesso) - t0 ))" "${semi:-?}"
    echo "  registro: $registro"
  fi
  i=$((i + 1))
done

# il riepilogo: la caccia è finita, non interrotta
echo "caccia finita: $(( $(adesso) - inizio ))s in tutto, $rossi rossi, registro in $LOGDIR"
if [ "$rossi" -gt 0 ]; then
  echo "c'è almeno un rosso: il giudizio (difetto dichiarato o nuovo) sta nel REPORT, con la riproduzione del seme"
  exit 1
fi
exit 0
