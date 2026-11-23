#!/bin/sh
# v0.6.14 (N3): la caccia a LIVELLI — questo è il LIVELLO 2, il cancello
# della fase. Il livello 1 è la suite a 128 casi coi file delle regressioni,
# a ogni commit (com'è); il livello 3 è scripts/caccia-notte.sh, fuori dal
# percorso critico. I tre livelli, i budget e lo smistamento dei rossi
# stanno in docs/caccia.md.
#
# COSA CAMBIA dalla caccia della v0.6.6–v0.6.13 (20 000 casi a testa, a
# ogni proprietà: 6 h 33 min, rifatti):
#
# 1. BUDGET DI TEMPO, non numero di casi. Ogni proprietà riceve un tempo;
#    lo script calcola i casi dal costo per caso misurato nella caccia
#    prima (i registri in target/caccia/<inizio>/misura.tsv). Senza una
#    misura parte con una corsa breve di calibrazione (MIN_CASI). Mai meno
#    di MIN_CASI, mai più di MAX_CASI. Il registro dice quanti casi sono
#    DAVVERO corsi (proptest stampa un rigo per caso: PROPTEST_VERBOSE=2).
#
# 2. PIÙ TEMPO DOVE LA FASE HA TOCCATO IL CODICE. Una proprietà è TOCCATA
#    se dalla base della fase è cambiato uno dei moduli che esercita,
#    oppure il suo file di test — il suo oracolo: un cambio di oracolo
#    obbliga a rifare la caccia di quella proprietà (nella v0.6.13 il
#    giudice della quarantena cambiò dopo la caccia). La mappa proprietà →
#    moduli è la terza colonna dell'inventario (properties.txt), guardata
#    da `properties_inventory`. Le toccate prendono TOCCATE minuti, le
#    altre ALTRE. Se la somma supera l'obiettivo i budget si riducono in
#    proporzione — prima le toccate, mai sotto il budget delle altre. Chi
#    raggiunge MAX_CASI senza esaurire il suo budget (una proprietà che
#    costa poco) prende il massimo dei casi e non si riduce: il massimo
#    costa quello che costa, ed è già contato.
#
# 3. UNA corsa sola della suite a SUITE_CASI, non cinque.
#
# 4. LA REGOLA DEL COMMIT FINALE resta, più leggera: un commit dopo il
#    cancello che tocca crates/*/src, o l'oracolo di una proprietà,
#    obbliga a rifare il cancello SOLO per le proprietà toccate da quel
#    commit:
#        CACCIABASE=<commit del cancello> CACCIASOLO="…" scripts/caccia.sh
#
# I difetti APERTI, quando ci sono, stanno in
# crates/engramma-core/tests/open-defects.txt e le loro proprietà si
# girano COMUNQUE: un difetto dichiarato non è un motivo per non giudicare
# il resto (il file esiste solo con almeno un difetto aperto: dalla
# v0.6.14 non c'è più). Il giudizio su un rosso — «nasce da questa fase o
# c'era già?» — ha un budget di 30 minuti e tre passi meccanici
# (docs/caccia.md): la riproduzione deterministica del seme (ogni
# proprietà ha un corpo `fn <nome>_body(seed: u64)`), lo stesso seme sul
# tag precedente (scripts/seme-sul-tag.sh), il giudizio.
#
# Uso:  scripts/caccia.sh                  # il cancello intero
#       CACCIABASE=<ref> scripts/caccia.sh # la base della fase (default:
#                                         # l'ultimo tag fase-v* antenato
#                                         # di HEAD)
#       CACCIASOLO="a b" scripts/caccia.sh # solo queste proprietà
#
# Parametri (variabili d'ambiente, tutti con un default):
#   OBIETTIVO=100       minuti di tutto, suite compresa (l'obiettivo della
#                       fase è stare sotto 1 h 45 min)
#   TOCCATE=10          minuti per una proprietà toccata
#   ALTRE=2             minuti per le altre
#   MIN_CASI=256        MAX_CASI=20000
#   SUITE_CASI=2048     casi della corsa della suite (0: nessuna corsa)
#   SUITE_RISERVA=1500  secondi riservati alla suite senza una misura
#
# Tutto gira in release, a priorità minima (nice 19, due job di cargo,
# due fili di test), mai due corse pesanti insieme, e su macOS sotto
# caffeinate -i. Una corsa che sfora il suo budget oltre il margine viene
# fermata (esito TEMPO): una stima sbagliata non deve trascinarsi dietro
# il cancello. Esce con un codice diverso da zero se c'è almeno un rosso.

OBIETTIVO=${OBIETTIVO:-100}
TOCCATE=${TOCCATE:-10}
ALTRE=${ALTRE:-2}
MIN_CASI=${MIN_CASI:-256}
MAX_CASI=${MAX_CASI:-20000}
SUITE_CASI=${SUITE_CASI:-2048}
SUITE_RISERVA=${SUITE_RISERVA:-1500}
CACCIASOLO=${CACCIASOLO:-}
CACCIABASE=${CACCIABASE:-}

# su macOS la corsa dura ore: il sonno del Mac non la interrompe
if [ "$(uname)" = Darwin ] && command -v caffeinate >/dev/null 2>&1 \
   && [ -z "${CACCIANO_CAFFE:-}" ]; then
  exec caffeinate -i env CACCIANO_CAFFE=1 "$0" "$@"
fi

export CARGO_BUILD_JOBS=2
export RUST_TEST_THREADS=2
# un rigo per caso (il livello 2 di proptest): il registro dice quanti
# casi sono corsi davvero, anche a una corsa che cade
export PROPTEST_VERBOSE=2

INVENTARIO=crates/engramma-core/tests/properties.txt
# i difetti aperti dichiarati: il file C'È solo quando c'è almeno un difetto
# aperto (v0.6.14: l'ultimo dichiarato era chiuso dalla v0.6.9 — il file è
# andato via con lui, come il suo cancello prescriveva)
APERTI=crates/engramma-core/tests/open-defects.txt
[ -f "$INVENTARIO" ] || { echo "manca $INVENTARIO" >&2; exit 1; }
command -v perl >/dev/null 2>&1 || { echo "serve perl (il limite di tempo)" >&2; exit 1; }

cd "$(dirname "$0")/.." || exit 1

adesso() { date +%s; }
INIZIO=$(date +%Y%m%d-%H%M%S)
LOGDIR=target/caccia/$INIZIO
mkdir -p "$LOGDIR"
REGISTRO="$LOGDIR/misura.tsv"
inizio=$(adesso)
TAB="$(printf '\t')"

# l'inventario dev'essere onesto PRIMA di sprecare ore (e con lui la mappa
# dei moduli: ogni proprietà la sua, ogni percorso che esiste)
nice -n 19 cargo test --release -p engramma-core --test properties_inventory \
  >/dev/null || { echo "l'inventario non torna (mappa dei moduli, #[test] di ogni proprietà)" >&2; exit 1; }
# le corse sotto misurano il TEST, non la compilazione: si costruisce prima
nice -n 19 cargo test --release -p engramma-core -p engramma-mcp --no-run \
  >"$LOGDIR/build.log" 2>&1 || { echo "la build in release è fallita: $LOGDIR/build.log" >&2; exit 1; }

# la base della fase: l'ultimo tag fase-v* che HEAD contiene
if [ -z "$CACCIABASE" ]; then
  CACCIABASE=$(git tag --merged HEAD --list 'fase-v*' --sort=-creatordate | head -n 1)
fi
[ -n "$CACCIABASE" ] || { echo "nessun tag fase-v* antenato di HEAD: dai CACCIABASE=<ref>" >&2; exit 2; }
CAMBIATI="$LOGDIR/cambiati.txt"
{
  git diff --name-only "$CACCIABASE" HEAD
  git diff --name-only HEAD
  git ls-files --others --exclude-standard
} | sort -u >"$CAMBIATI"

# il costo per caso dell'ULTIMA caccia che l'ha misurato (una corsa verde:
# casi corsi e secondi), cercando all'indietro nei registri
COSTI="$LOGDIR/costi.txt"
for d in $(ls -dt target/caccia/*/ 2>/dev/null | grep -v "^$LOGDIR/"); do
  [ -f "${d}misura.tsv" ] || continue
  awk -F'\t' '$1=="prop" && $7!="" && $7!="0" {print $2 "\t" $7}' "${d}misura.tsv"
done | awk -F'\t' '!v[$1]++' >"$COSTI"
SUITE_MISURATA=$(for d in $(ls -dt target/caccia/*/ 2>/dev/null | grep -v "^$LOGDIR/"); do
  [ -f "${d}misura.tsv" ] || continue
  awk -F'\t' '$1=="suite" && $6!="" {print $6; exit}' "${d}misura.tsv"
done | head -n 1)
[ -n "$SUITE_MISURATA" ] || SUITE_MISURATA=$SUITE_RISERVA

# Un comando sotto un LIMITE DI SECONDI: allo scadere muore con tutto il
# suo gruppo (cargo, rustc, il binario di test), e l'esito è 124 — come
# `timeout`. Nessuna cancellazione: le cartelle di test che lascia le
# raccoglie lo spazzino della suite dopo (quelle nostre, stantie).
con_limite() {
  limite=$1
  shift
  perl -e '
    my $limite = shift;
    my $pid = fork;
    die "fork: $!" unless defined $pid;
    # il FIGLIO apre un gruppo suo: allo scadere muore con tutto ciò che ha
    # generato (cargo, rustc, il binario di test), e il supervisore resta
    # fuori a riferire come e` andata
    if ($pid == 0) {
      setpgrp(0, 0) or die "setpgrp: $!";
      exec @ARGV;
      die "exec: $!";
    }
    my $scaduto = 0;
    my $uccidi = sub {
      if (!$scaduto) {
        $scaduto = 1;
        kill "TERM", -$pid;
        $SIG{ALRM} = sub { kill "KILL", -$pid };
        alarm 20;
      } else {
        kill "KILL", -$pid;
      }
    };
    $SIG{ALRM} = $uccidi;
    # se è lo script a morire (Ctrl-C), il figlio non resta orfano
    $SIG{INT} = $SIG{TERM} = sub { kill "KILL", -$pid; exit 130 };
    alarm $limite;
    1 while (waitpid($pid, 0) == -1 && $!{EINTR});
    my $st = $?;
    exit(124) if $scaduto;
    exit($st & 127 ? 128 + ($st & 127) : $st >> 8);
  ' "$limite" "$@"
}

# quanti casi proptest dice aver fatto (un rigo per caso al livello 2)
casi_corsi() {
  n=$(grep -cE 'Test case (passed|rejected|failed)' "$1" 2>/dev/null || true)
  echo "${n:-0}"
}

semi_di() {
  # il seme che cade è l'INGRESSO del caso minimo, quello che `<nome>_body`
  # rigioca: lo scrive proptest nella sua diagnosi («input: N» — il Debug
  # dell'errore); in mancanza, l'ingresso dell'ultimo caso caduto (al
  # livello 2 proptest stampa l'ingresso di ogni caso: «Next test input:
  # seed = N», che su una corsa VERDE non è un seme che cade)
  {
    grep -Eo 'input: [0-9]+' "$1" 2>/dev/null | sed 's/^input: /seed = /'
    awk '/^proptest: Next test input: seed = / { s = $NF }
         /^proptest: Test case failed/ { print "seed = " s }' "$1" 2>/dev/null
  } | awk '!v[$0]++' | head -3 | tr '\n' ' ' | sed 's/ *$//'
}

: >"$REGISTRO"
rossi=0

NP=$(grep -cve '^#' -e '^$' "$INVENTARIO")
TOCCATE_N=$(grep -v '^#' "$INVENTARIO" | grep -v '^$' | while read -r file nome moduli; do
  for m in $file $(echo "$moduli" | tr ',' ' '); do
    grep -qxF "$m" "$CAMBIATI" && { echo x; break; }
  done
done | wc -l | tr -d ' ')
echo "caccia: fino a $NP proprietà dell'inventario, base $CACCIABASE, $TOCCATE_N toccate, obiettivo $OBIETTIVO minuti (suite compresa)"
if [ -f "$APERTI" ]; then
  echo "difetti aperti dichiarati (si girano comunque):"
  sed -e 's/#.*//' -e '/^$/d' -e '/^#/d' "$APERTI" | while read -r file nome semi _x; do
    [ -n "$file" ] || continue
    echo "  - $nome: $semi"
  done
  echo "i semi dei difetti aperti stanno fuori dai file delle regressioni (controllato dall'inventario)"
else
  echo "nessun difetto aperto dichiarato"
fi
echo "file cambiati dalla base: $(wc -l <"$CAMBIATI" | tr -d ' ') — $CAMBIATI"

# --- il piano: una riga per proprietà -----------------------------------
# nome, pacchetto, bersaglio, toccata (1/0), costo per caso in µs (vuoto:
# da calibrare)
PIANO="$LOGDIR/piano.tsv"
: >"$PIANO"
grep -v '^#' "$INVENTARIO" | grep -v '^$' | while read -r file nome moduli; do
  [ -n "$file" ] || continue
  if [ -n "$CACCIASOLO" ] && ! echo " $CACCIASOLO " | grep -q " $nome "; then
    continue
  fi
  pacchetto=$(basename "$(dirname "$(dirname "$file")")")
  bersaglio=$(basename "$file" .rs)
  toccata=0
  for m in $file $(echo "$moduli" | tr ',' ' '); do
    if grep -qxF "$m" "$CAMBIATI"; then toccata=1; break; fi
  done
  costo=$(awk -F'\t' -v n="$nome" '$1==n {print $2; exit}' "$COSTI")
  printf '%s\t%s\t%s\t%s\t%s\n' "$nome" "$pacchetto" "$bersaglio" "$toccata" "${costo:-}" >>"$PIANO"
done

# --- la calibrazione di chi non ha una misura ---------------------------
PIANO2="$LOGDIR/piano2.tsv"
: >"$PIANO2"
while IFS="$TAB" read -r nome pacchetto bersaglio toccata costo; do
  if [ -z "$costo" ]; then
    registro="$LOGDIR/calibrazione-$nome.log"
    printf '%s ' "calibrazione $nome"
    t0=$(adesso)
    con_limite $((MIN_CASI * 30 + 600)) env PROPTEST_CASES=$MIN_CASI \
      nice -n 19 cargo test --release -p "$pacchetto" --test "$bersaglio" "$nome" -- --nocapture \
      >"$registro" 2>&1
    st=$?
    t=$(($(adesso) - t0))
    corsi=$(casi_corsi "$registro")
    semi=$(semi_di "$registro")
    if [ "$st" -eq 0 ]; then
      [ "$corsi" -gt 0 ] || corsi=$MIN_CASI
      costo=$(awk -v s="$t" -v c="$corsi" 'BEGIN { printf "%d", s * 1000000 / c }')
      [ "$costo" -gt 0 ] || costo=1
      echo "$t s, $corsi casi, $(awk -v c="$costo" 'BEGIN { printf "%.3f", c / 1000000 }') s per caso"
    elif [ "$st" -eq 124 ]; then
      echo "TEMPO dopo $t s ($corsi casi corsi)"
      echo "  registro: $registro"
      printf 'prop\t%s\t%s\t0\t%s\t%s\t\tTEMPO\t%s\n' "$nome" "$toccata" "$corsi" "$t" "$semi" >>"$REGISTRO"
      rossi=$((rossi + 1))
      costo=-1
    else
      echo "ROSSO in calibrazione, $t s, semi: ${semi:-?}"
      echo "  registro: $registro"
      printf 'prop\t%s\t%s\t0\t%s\t%s\t\tROSSO\t%s\n' "$nome" "$toccata" "$corsi" "$t" "$semi" >>"$REGISTRO"
      rossi=$((rossi + 1))
      costo=-1
    fi
  fi
  printf '%s\t%s\t%s\t%s\t%s\n' "$nome" "$pacchetto" "$bersaglio" "$toccata" "$costo" >>"$PIANO2"
done <"$PIANO"

# --- i budget: il tempo dove la fase ha toccato --------------------------
# Il costo di una proprietà mai misurata (fermata o rossa in calibrazione)
# resta -1: è già giudicata, e la corsa sotto la salta.
PIANO3="$LOGDIR/piano3.tsv"
awk -F'\t' -v OFS='\t' \
  -v obiettivo="$OBIETTIVO" -v suite="$SUITE_MISURATA" \
  -v toccate_min="$TOCCATE" -v altre_min="$ALTRE" \
  -v minc="$MIN_CASI" -v maxc="$MAX_CASI" '
  { n++; nome[n]=$1; pkg[n]=$2; tgt[n]=$3; toc[n]=$4; cost[n]=$5 }
  END {
    pool = obiettivo * 60 - suite
    if (pool < 0) pool = 0
    su = 0; st = 0; fisso = 0
    for (i = 1; i <= n; i++) {
      b[i] = (toc[i] == 1 ? toccate_min : altre_min) * 60
      capo[i] = -1
      if (cost[i] > 0) {
        capo[i] = maxc * cost[i] / 1000000     # i secondi di MAX_CASI casi
        if (capo[i] <= b[i]) fisso += capo[i]  # poco costosa: il massimo, intero
        else if (toc[i] == 1) st += b[i]
        else su += b[i]
      }
    }
    fu = 1; ft = 1
    rest = pool - fisso
    if (su > rest && su > 0) fu = rest / su
    rest = rest - su * fu
    if (st > rest && st > 0) ft = rest / st
    for (i = 1; i <= n; i++) {
      if (cost[i] <= 0) { bud = 0; casi = 0 }
      else if (capo[i] <= b[i]) { bud = capo[i]; casi = maxc }
      else {
        bud = b[i] * (toc[i] == 1 ? ft : fu)
        casi = int(bud * 1000000 / cost[i] + 0.5)
        if (casi < minc) casi = minc
        if (casi > maxc) casi = maxc
      }
      print nome[i], pkg[i], tgt[i], toc[i], cost[i], int(bud), casi
    }
  }' "$PIANO2" >"$PIANO3"
[ -s "$PIANO3" ] || echo "nessuna proprietà da girare: CACCIASOLO nomina proprietà fuori dell'inventario?" >&2

# --- le corse -------------------------------------------------------------
while IFS="$TAB" read -r nome pacchetto bersaglio toccata costo budget casi; do
  # già giudicata in calibrazione (rossa o fermata): non si rigira
  if grep -q "^prop${TAB}${nome}${TAB}" "$REGISTRO"; then continue; fi
  printf '%s ' "$nome"
  registro="$LOGDIR/prop-$nome.log"
  # il margine: metà del budget e due minuti; mai meno di cinque minuti
  cap=$((budget + budget / 2 + 120))
  [ "$cap" -ge 300 ] || cap=300
  t0=$(adesso)
  con_limite "$cap" env PROPTEST_CASES="$casi" nice -n 19 cargo test --release \
    -p "$pacchetto" --test "$bersaglio" "$nome" -- --nocapture >"$registro" 2>&1
  st=$?
  t=$(($(adesso) - t0))
  corsi=$(casi_corsi "$registro")
  semi=$(semi_di "$registro")
  costo_us=""
  if [ "$st" -eq 0 ]; then
    esito=verde
    [ "$corsi" -gt 0 ] || corsi=$casi
    costo_us=$(awk -v s="$t" -v c="$corsi" 'BEGIN { printf "%d", s * 1000000 / c }')
    printf 'verde %ss (%s, toccata=%s, budget %ss, %s casi)\n' "$t" "$pacchetto/$nome" "$toccata" "$budget" "$corsi"
  elif [ "$st" -eq 124 ]; then
    esito=TEMPO
    rossi=$((rossi + 1))
    printf 'TEMPO %ss (%s, fermata al limite di %ss: la stima del costo era troppo bassa)\n' "$t" "$pacchetto/$nome" "$cap"
    echo "  registro: $registro"
  else
    esito=ROSSO
    rossi=$((rossi + 1))
    printf 'ROSSO %ss (%s) semi: %s\n' "$t" "$pacchetto/$nome" "${semi:-?}"
    echo "  registro: $registro"
  fi
  printf 'prop\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$nome" "$toccata" "$budget" "$corsi" "$t" "$costo_us" "$esito" "$semi" >>"$REGISTRO"
done <"$PIANO3"

# --- la corsa della suite (una) --------------------------------------------
if [ "$SUITE_CASI" -gt 0 ]; then
  printf 'suite (%s casi) ' "$SUITE_CASI"
  registro="$LOGDIR/suite.log"
  cap=$((SUITE_MISURATA + SUITE_MISURATA / 2 + 300))
  t0=$(adesso)
  con_limite "$cap" env PROPTEST_CASES=$SUITE_CASI nice -n 19 cargo test --release \
    -p engramma-core >"$registro" 2>&1
  st=$?
  t=$(($(adesso) - t0))
  semi=$(semi_di "$registro")
  if [ "$st" -eq 0 ]; then
    esito=verde
    printf 'verde %ss\n' "$t"
  elif [ "$st" -eq 124 ]; then
    esito=TEMPO
    rossi=$((rossi + 1))
    printf 'TEMPO %ss (fermata al limite di %ss)\n' "$t" "$cap"
  else
    esito=ROSSO
    rossi=$((rossi + 1))
    printf 'ROSSO %ss semi: %s\n' "$t" "${semi:-?}"
    echo "  registro: $registro"
  fi
  printf 'suite\tsuite\t-\t%s\t-\t%s\t-\t%s\t%s\n' "$SUITE_CASI" "$t" "$esito" "$semi" >>"$REGISTRO"
fi

# --- il riepilogo: la caccia è finita, non interrotta ----------------------
echo
echo "registro: $REGISTRO — proprietà, toccata, budget (s), casi corsi, secondi, esito, semi"
awk -F'\t' '{
  if ($1 == "suite") printf "%-44s %-8s %8s %8s %7s %s %s\n", "suite del nucleo", "-", "-", $4, $6 "s", $8, $9
  else printf "%-44s %-8s %8s %8s %7s %s %s\n", $2, ($3==1 ? "sì" : "no"), $4 "s", $5, $6 "s", $8, $9
}' "$REGISTRO"
echo
echo "caccia finita: $(( $(adesso) - inizio ))s in tutto, $rossi rossi, registro in $LOGDIR (misura.tsv)"
if [ "$rossi" -gt 0 ]; then
  echo "c'è almeno un rosso: lo smistamento ha 30 minuti per tre passi (docs/caccia.md) — la riproduzione del seme, scripts/seme-sul-tag.sh, il giudizio"
  exit 1
fi
exit 0
