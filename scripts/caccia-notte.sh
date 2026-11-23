#!/bin/sh
# v0.6.14 (N3): la caccia NOTTURNA — il LIVELLO 3. Fuori dal percorso
# critico: una corsa lunga e continua, come il fuzzing continuo di OSS-Fuzz
# e della CI di FoundationDB — un cancello breve e mirato per la fase
# (scripts/caccia.sh), la caccia profonda di notte, e quello che trova
# diventa una regressione o un difetto dichiarato (docs/caccia.md).
#
# COSA FA. Gira su un ramo (default main), a nice 19 con due fili, in una
# finestra di ore (default dalle 01:00 alle 07:00), e si ferma da sola
# alla fine della finestra. Le proprietà girano a rotazione, a partire
# dalle più costose, così ogni proprietà prende una corsa lunga ogni
# pochi giorni. I SEMI CAMBIANO OGNI NOTTE: la variabile d'ambiente di
# proptest che semina il generatore è PROPTEST_RNG_SEED (proptest 1.12.0;
# verificata nel sorgente della versione in uso), e lo script la deriva
# dalla data e dalla proprietà — una notte si rifà con lo stesso seme.
# Scrive i risultati in target/caccia-notte/<data>/: un file semi.txt con
# proprietà, seme e prima riga del messaggio, e il registro di ogni corsa.
#
# NON COMMITTA NIENTE E NON TOCCA IL REPO: scrive solo sotto target/, e
# spegne la persistenza dei file di regressione di proptest
# (PROPTEST_DISABLE_FAILURE_PERSISTENCE) — un rosso di notte non riscrive
# i .proptest-regressions del ramo. Un rosso si smista il giorno dopo con
# lo stesso budget del cancello (30 minuti, tre passi).
#
# L'INSTALLAZIONE È DELL'UTENTE. Questo script e il launchd di esempio
# (scripts/it.engramma.caccia-notte.plist) stanno nel repo; nulla li
# installa. Far girare qualcosa ogni notte sulla macchina è una decisione
# dell'utente: le istruzioni in docs/caccia.md.
#
# Uso:  scripts/caccia-notte.sh                        # la finestra 01:00–07:00
#       FINESTRA="01:00 07:00" scripts/caccia-notte.sh  # un'altra finestra
#       DURATA=20 scripts/caccia-notte.sh                # una PROVA: da adesso per 20 minuti
# Parametri (variabili d'ambiente):
#   FINESTRA="01:00 07:00"  dalle–alle, ora locale (attraversa mezzanotte)
#   DURATA=<minuti>         in vez della finestra: parte adesso, si ferma dopo
#   FETTA=40                minuti per proprietà
#   MAX_CASI=200000         il tetto dei casi di una corsa (lo ferma la fetta)
#   RAMO=main               il ramo su cui girare (altrimenti si rifiuta)

FINESTRA=${FINESTRA:-"01:00 07:00"}
DURATA=${DURATA:-}
FETTA=${FETTA:-40}
MAX_CASI=${MAX_CASI:-200000}
RAMO=${RAMO:-main}

if [ "$(uname)" = Darwin ] && command -v caffeinate >/dev/null 2>&1 \
   && [ -z "${CACCIANO_CAFFE:-}" ]; then
  exec caffeinate -i env CACCIANO_CAFFE=1 "$0" "$@"
fi

export CARGO_BUILD_JOBS=2
export RUST_TEST_THREADS=2
export PROPTEST_VERBOSE=2
export PROPTEST_DISABLE_FAILURE_PERSISTENCE=1

command -v perl >/dev/null 2>&1 || { echo "serve perl (il limite di tempo)" >&2; exit 1; }

cd "$(dirname "$0")/.." || exit 1
INVENTARIO=crates/engramma-core/tests/properties.txt
[ -f "$INVENTARIO" ] || { echo "manca $INVENTARIO" >&2; exit 1; }

# solo sul ramo deciso: la notte giudica il ramo, non un albero a metà
RAMO_ADESSO=$(git rev-parse --abbrev-ref HEAD 2>/dev/null)
[ "$RAMO_ADESSO" = "$RAMO" ] || { echo "la caccia notturna gira su «${RAMO}», non su «${RAMO_ADESSO}»: niente da fare" >&2; exit 2; }

adesso() { date +%s; }
minuti() { date +%k%M | awk '{ print $1 + 0 }'; }   # l'ora di adesso in minuti
in_minuti() { echo "$1" | awk -F: '{ print ($1 * 60) + $2 }'; }

# la fine della finestra, in secondi da adesso
if [ -n "$DURATA" ]; then
  case $DURATA in '' | *[!0-9]*) echo "DURATA è un numero di minuti" >&2; exit 2 ;; esac
  FINE=$(( $(adesso) + DURATA * 60 ))
  FINESTRA_TESTO="da adesso per $DURATA minuti"
else
  DA=$(in_minuti "$(echo $FINESTRA | awk '{ print $1 }')")
  A=$(in_minuti "$(echo $FINESTRA | awk '{ print $2 }')")
  ADESSO=$(minuti)
  if [ "$ADESSO" -lt "$DA" ]; then
    ATTESA=$(( (DA - ADESSO) * 60 ))
    echo "la finestra comincia alle $(echo $FINESTRA | awk '{ print $1 }'): aspetto $ATTESA s"
    sleep "$ATTESA"
  elif [ "$ADESSO" -ge "$A" ] && [ "$A" -ge "$DA" ]; then
    echo "la finestra di oggi ($FINESTRA) è finita: niente da fare"
    exit 0
  fi
  FINE=$(( $(adesso) + (A - $(minuti)) * 60 ))
  FINESTRA_TESTO="finestra $FINESTRA"
fi
# l'ora di adesso può scavalcare la mezzanotte dentro una finestra che la
# attraversa: la fine non è mai oltre la lunghezza della finestra
LUNGHEZZA=$(( (A - DA + 1440) % 1440 ))
[ -z "$DURATA" ] && [ "$FINE" -gt $(( $(adesso) + LUNGHEZZA * 60 )) ] \
  && FINE=$(( $(adesso) + LUNGHEZZA * 60 ))

DATA=$(date +%Y%m%d)
GIORNO=$(date +%Y%m%d | awk '{ print $1 + 0 }')
OUT=target/caccia-notte/$DATA
mkdir -p "$OUT"
SEMI="$OUT/semi.txt"

# Un comando sotto un LIMITE DI SECONDI (come in scripts/caccia.sh): allo
# scadere muore con tutto il suo gruppo, esito 124.
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

prima_riga() {
  # la prima riga del MESSAGGIO di caduta: la prima riga non vuota dopo il
  # «panicked at …:»; senza un panic (una corsa che non compila), la prima
  # riga con «error»; «-» a una corsa verde
  awk '
    /panicked at / { p = 1; next }
    p && NF { print; exit }
    !p && /^error/ && !e { e = 1; print; exit }
  ' "$1" 2>/dev/null | sed 's/^ *//' | cut -c1-200
}

# la rotazione: le proprietà dalla più costosa alla più economica (il costo
# dell'ultima caccia che l'ha misurato; chi non ha una misura sta in fondo,
# nell'ordine dell'inventario), e ogni notte si parte più avanti
ORDINE="$OUT/ordine.txt"
: >"$ORDINE"
for d in $(ls -dt target/caccia/*/ 2>/dev/null); do
  [ -f "${d}misura.tsv" ] || continue
  awk -F'\t' '$1 == "prop" && $7 != "" && $7 != "0" { print $7 "\t" $2 }' "${d}misura.tsv"
done | awk -F'\t' '!v[$2]++' | sort -rn | cut -f2 >"$ORDINE"
grep -v '^#' "$INVENTARIO" | grep -v '^$' | awk '{ print $2 }' | while read -r n; do
  grep -qxF "$n" "$ORDINE" || echo "$n" >>"$ORDINE"
done
N=$(wc -l <"$ORDINE" | tr -d ' ')
PER_NOTTE=$(( (LUNGHEZZA / FETTA) > 0 ? LUNGHEZZA / FETTA : 1 ))
[ -n "$DURATA" ] && PER_NOTTE=$(( (DURATA / FETTA) > 0 ? DURATA / FETTA : 1 ))
# il giorno dice da dove si parte: la rotazione torna al principio ogni
# N notti
PARTENZA=$(( (GIORNO * PER_NOTTE) % N ))

{
  echo "# caccia notturna del $DATA — $FINESTRA_TESTO, $FETTA minuti per proprietà"
  printf '# proprietà\tseme (PROPTEST_RNG_SEED)\tprima riga del messaggio («-»: verde)\n'
} >"$SEMI"

# si costruisce prima: le corse misurano il test
nice -n 19 cargo test --release -p engramma-core -p engramma-mcp --no-run \
  >"$OUT/build.log" 2>&1 || { echo "la build in release è fallita: $OUT/build.log" >&2; exit 1; }

i=0
while [ "$i" -lt "$N" ]; do
  if [ "$(adesso)" -ge "$FINE" ]; then break; fi
  nome=$(sed -n "$(( (PARTENZA + i) % N + 1 ))p" "$ORDINE")
  file=$(awk -v n="$nome" '!/^#/ && $2 == n { print $1; exit }' "$INVENTARIO")
  [ -n "$file" ] && [ -n "$nome" ] || { i=$((i + 1)); continue; }
  pacchetto=$(basename "$(dirname "$(dirname "$file")")")
  bersaglio=$(basename "$file" .rs)
  resto=$(( FINE - $(adesso) ))
  # la finestra è finita fra un controllo e l'altro — e un limite di zero
  # secondi spegnerebbe l'allarme: mai una corsa senza meno di un minuto
  [ "$resto" -ge 60 ] || break
  fetta=$(( FETTA * 60 ))
  [ "$resto" -lt "$fetta" ] && fetta=$resto
  # il seme della notte: la data e la posizione della proprietà (proptest
  # semina il generatore con PROPTEST_RNG_SEED)
  seme=$(awk -v g="$GIORNO" -v i="$i" 'BEGIN { printf "%d", (g * 1000003 + i * 7919) % 9007199254740992 }')
  registro="$OUT/notte-$nome.log"
  printf '%s (%s s) … ' "$nome" "$fetta"
  t0=$(adesso)
  con_limite "$fetta" env PROPTEST_RNG_SEED="$seme" PROPTEST_CASES=$MAX_CASI \
    nice -n 19 cargo test --release -p "$pacchetto" --test "$bersaglio" "$nome" -- --nocapture \
    >"$registro" 2>&1
  st=$?
  t=$(($(adesso) - t0))
  casi=$(grep -cE 'Test case (passed|rejected|failed)' "$registro" 2>/dev/null || true)
  if [ "$st" -eq 0 ]; then
    esito=verde; messaggio="-"
  elif [ "$st" -eq 124 ]; then
    esito="fine della fetta"; messaggio="-"
  else
    esito=ROSSO; messaggio=$(prima_riga "$registro")
  fi
  printf '%s, %s casi in %ss%s\n' "$esito" "${casi:-0}" "$t" "$([ "$esito" = ROSSO ] && echo " — $registro")"
  printf '%s\t%s\t%s\n' "$nome" "$seme" "$messaggio" >>"$SEMI"
  [ "$esito" = ROSSO ] && printf 'rossa: %s (seme del generatore %s) — %s\n' "$nome" "$seme" "$registro" >>"$OUT/rossi.txt"
  i=$((i + 1))
done

rossi=$( [ -f "$OUT/rossi.txt" ] && wc -l <"$OUT/rossi.txt" | tr -d ' ' || echo 0 )
echo "caccia notturna finita: $i proprietà, $rossi rossi — $SEMI"
[ "$rossi" -eq 0 ] || exit 1
