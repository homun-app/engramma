#!/bin/sh
# v0.6.14 (N3): LO STESSO SEME SUL TAG — il secondo passo dello smistamento
# di un rosso (docs/caccia.md). Un rosso del cancello o della notte ha un
# budget di 30 minuti per tre passi meccanici: la riproduzione
# deterministica del seme, LO STESSO SEME SUL TAG PRECEDENTE (questo
# script), il giudizio — nasce da questa fase si corregge; c'era già si
# dichiara aperto in open-defects.txt col seme e il verdetto di qui, e si
# va avanti. La diagnosi della causa di un difetto pre-esistente non
# spetta alla fase che lo trova: la fa la fase che lo chiude.
#
# Come lavora: un worktree del tag DENTRO target/ (mai in $TMPDIR), che si
# costruisce NELLA SUA CARTELLA DI BUILD (il `target` del worktree, che se
# ne va con lui); nel file di test della proprietà, nel worktree, un test
# che chiama il corpo della proprietà col seme; una corsa; il verdetto; il
# worktree se ne va da solo alla fine, anche se il test fallisce. Il repo
# non cambia: il test vive nel worktree e muore con lui.
#
# PERCHÉ NON LA CARGO_TARGET_DIR DEL REPO. Il brief della fase la chiedeva
# condivisa (le dipendenze sono già costruite), e così fu fino alla fine
# della fase — finché la stessa trappola, incontrata nelle misure, non ha
# fatto ricontrollare. Due alberi diversi nella stessa cartella di build
# danno i manufatti CON LO STESSO NOME (cargo non mette il percorso nel
# nome), e cargo giudica «fresco» un manufatto se i sorgenti CHE HA
# ELENCATO L'ULTIMA VOLTA non sono cambiati da allora: non guarda l'albero
# che gli sta davanti. Costruita così, la seconda «build» di due alberi
# non compilava niente e i due binari erano identici al byte (REPORT,
# v0.6.14, «Le misure»). Qui il rischio era di provare il motore di OGGI
# col file di test del tag — e «cade anche sul tag» e il motore di oggi
# che cade dicono la stessa cosa. Ogni verdetto della fase è stato rifatto
# con questo script. Costa una build intera a corsa (pochi minuti): è il
# prezzo di un verdetto che significa quel che dice.
#
# DUE CONTROLLI, e senza di loro lo script non dà un verdetto:
#   1. la LIBRERIA del pacchetto risulta COMPILATA in questa corsa, dal
#      worktree (cargo, `--message-format=json`: un artefatto non fresco,
#      col manifesto dentro il worktree);
#   2. lo SCHEMA di una cella nuova, letto dal motore che gira, è quello
#      che il tag dichiara nel suo `schema.rs` (`SCHEMA_VERSION`).
# Il secondo, da solo, non distingue due tag con lo stesso schema: il
# primo, da solo, non guarda che cosa gira. Insieme, con la cartella di
# build tutta del worktree, sì.
#
# Da questa fase ogni proprietà ha un corpo `fn <nome>_body(seed: u64)`.
# Sui tag prima della v0.6.14 solo tre proprietà ne hanno uno
# (`purge_takes_only_its_family_body`, `frozen_prefix_body`,
# `quarantine_exits_body`, dalla v0.6.12–v0.6.13): per quelle il nome è
# lo stesso; per le altre il tag non ha corpo, e lo script lo dice — si
# riproduce il caso a mano su quel tag. Se un corpo ha un altro nome, dai
# quello come quarto argomento.
#
# Uso:  scripts/seme-sul-tag.sh <proprietà> <seme> <tag> [corpo]
# Esce: 0 = il seme NON cade sul tag (il difetto nasce da questa fase);
#       1 = il seme CADE ANCHE sul tag (c'era già);
#       2 = proprietà, seme o tag sbagliati, corsa non riuscita, o il
#           motore che ha risposto non è verificato per quello del tag;
#       3 = il tag non ha il corpo.

set -u

if [ $# -lt 3 ]; then
  sed -n '2,25p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
fi
PROPRIETA=$1
SEME=$2
TAG=$3
CORPO=${4:-${PROPRIETA}_body}

REPO=$(cd "$(dirname "$0")/.." && pwd) || exit 2

case $SEME in
  '' | *[!0-9]*) echo "il seme è un numero intero senza segno: «${SEME}»" >&2; exit 2 ;;
esac
git -C "$REPO" rev-parse -q --verify "$TAG^{commit}" >/dev/null 2>&1 \
  || { echo "il tag «${TAG}» non esiste in questo repo" >&2; exit 2; }

# il worktree: DENTRO target/, a un percorso fisso per tag. La sua
# cartella di build è `$WT/target`: nasce e muore con lui
WT="$REPO/target/seme-sul-tag/$TAG"
mkdir -p "$(dirname "$WT")"
LOGS="$REPO/target/seme-sul-tag/log"
mkdir -p "$LOGS"
LOG="$LOGS/$PROPRIETA-$SEME-su-$TAG.log"
JSON="$LOG.json"

pulisci() {
  git -C "$REPO" worktree remove --force "$WT" >/dev/null 2>&1 || rm -rf "$WT"
  git -C "$REPO" worktree prune >/dev/null 2>&1 || true
}
# il worktree se ne va da solo, su ogni uscita — anche se il test fallisce
trap pulisci EXIT INT TERM
rm -rf "$WT"
git -C "$REPO" worktree add --detach "$WT" "$TAG" >/dev/null 2>&1 \
  || { echo "impossibile creare il worktree di «${TAG}» in $WT" >&2; exit 2; }

# il file della proprietà: dall'inventario DEL TAG quando c'è, altrimenti
# da quello del repo
INVENTARIO="$WT/crates/engramma-core/tests/properties.txt"
[ -f "$INVENTARIO" ] || INVENTARIO="$REPO/crates/engramma-core/tests/properties.txt"
FILE=$(awk -v n="$PROPRIETA" '!/^#/ && $2 == n { print $1; exit }' "$INVENTARIO")
[ -n "$FILE" ] || { echo "«${PROPRIETA}» non è nell'inventario delle proprietà" >&2; exit 2; }
DEST="$WT/$FILE"
[ -f "$DEST" ] || { echo "sul tag «${TAG}» non c'è $FILE" >&2; exit 3; }
grep -q "fn $CORPO(" "$DEST" \
  || { echo "il tag «${TAG}» non ha il corpo «${CORPO}» in $FILE: un tag prima della v0.6.14 lo ha solo per tre proprietà — dai il nome del corpo come quarto argomento, o riproduci il caso a mano su quel tag" >&2; exit 3; }

# lo schema che il tag dichiara (il secondo controllo: sotto)
SCHEMA_ATTESO=$(grep -E 'const SCHEMA_VERSION: [a-z0-9]+ = [0-9]+' \
  "$WT/crates/engramma-core/src/cell/schema.rs" 2>/dev/null \
  | grep -oE '[0-9]+' | tail -n 1)

# il test: chiama il corpo col seme. Un corpo può rispondere () (fallisce
# con un panic) o Result (fallisce con Err): l'esito si legge uguale.
# Prima, lo schema di una cella nuova — lo stampa sempre, verde o rosso.
cat >>"$DEST" <<EOF

// v0.6.14 (N3): generato da scripts/seme-sul-tag.sh — non committare.
trait EsitoDelCorpo {
    fn esito(self) -> Result<(), String>;
}
impl EsitoDelCorpo for () {
    fn esito(self) -> Result<(), String> {
        Ok(())
    }
}
impl<E: std::fmt::Debug> EsitoDelCorpo for Result<(), E> {
    fn esito(self) -> Result<(), String> {
        self.map_err(|e| format!("{e:?}"))
    }
}
#[test]
fn zz_seme_sul_tag() {
    {
        // which ENGINE answers? The script reads this line and compares it
        // with the tag's own declaration
        let dir = tempfile::tempdir().expect(/* prova */ "la cartella di prova");
        let cell = engramma_core::Cell::open_bare(&dir.path().join("motore.sqlite"))
            .expect(/* prova */ "la cella di prova");
        let schema = cell
            .debug_column("SELECT value FROM meta WHERE key = 'schema_version'")
            .expect(/* prova */ "lo schema");
        eprintln!(
            "ENGRAMMA_SCHEMA={}",
            schema.first().cloned().unwrap_or_default()
        );
    }
    if let Err(messaggio) = $CORPO($SEME).esito() {
        panic!("il seme cade: {messaggio}");
    }
}
EOF

pacchetto=$(basename "$(dirname "$(dirname "$FILE")")")
bersaglio=$(basename "$FILE" .rs)

echo "«${PROPRIETA}», seme $SEME, sul tag $TAG ($TAG → $(git -C "$REPO" rev-parse --short "$TAG^{commit}")), corpo $CORPO"
echo "build nella cartella del worktree ($WT/target): intera, pochi minuti"
inizio=$(date +%s)
(
  cd "$WT" || exit 2
  # nessuna CARGO_TARGET_DIR: la cartella di build è quella del worktree.
  # v0.6.19: anche quella dell'AMBIENTE va via — una cache condivisa ereditata
  # dalla sessione portava la build fuori dal worktree, che è la trappola
  # descritta in testa
  unset CARGO_TARGET_DIR
  CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 \
    PROPTEST_DISABLE_FAILURE_PERSISTENCE=1 \
    nice -n 19 cargo test -p "$pacchetto" --test "$bersaglio" zz_seme_sul_tag \
      --message-format=json-diagnostic-rendered-ansi -- --nocapture
) >"$JSON" 2>"$LOG"
st=$?
echo "build e corsa: $(($(date +%s) - inizio)) s"

# --- controllo 1: la libreria del pacchetto, compilata in QUESTA corsa, dal
# worktree. Un artefatto «fresco» è uno che cargo ha trovato già fatto
LIB=$(grep -F '"reason":"compiler-artifact"' "$JSON" | grep -F '"crate_types":["lib"]' \
  | grep -F "\"name\":\"$(echo "$pacchetto" | tr '-' '_')\"" \
  | grep -F "\"manifest_path\":\"$WT/" | grep -cF '"fresh":false')
if [ "$LIB" -lt 1 ]; then
  echo "il motore non è verificato: la libreria di «$pacchetto» non risulta compilata in questa corsa dal worktree (se la compilazione è fallita, il perché sta nei registri: $LOG e $JSON)" >&2
  grep -oE '"message":"[^"]{0,300}' "$JSON" | head -n 6 >&2
  tail -n 12 "$LOG" >&2
  exit 2
fi
# --- controllo 2: lo schema che il motore dichiara a runtime è quello del tag
SCHEMA_VISTO=$(grep -oE 'ENGRAMMA_SCHEMA=[0-9]+' "$LOG" | tail -n 1 | cut -d= -f2)
echo "motore: libreria compilata in questa corsa dal worktree; schema visto ${SCHEMA_VISTO:-?}, schema del tag ${SCHEMA_ATTESO:-?}"
if [ -z "$SCHEMA_VISTO" ] || [ -z "$SCHEMA_ATTESO" ]; then
  if grep -qE '^error' "$LOG" || ! grep -q 'Running ' "$LOG"; then
    echo "la corsa sul tag $TAG non è riuscita (compilazione o ambiente): guarda $LOG" >&2
    tail -n 20 "$LOG" >&2
    exit 2
  fi
  echo "il motore non è verificato: lo schema visto (${SCHEMA_VISTO:-nessuno}) o quello del tag (${SCHEMA_ATTESO:-ignoto}) manca (il registro: $LOG)" >&2
  exit 2
fi
if [ "$SCHEMA_VISTO" != "$SCHEMA_ATTESO" ]; then
  echo "il motore che ha risposto NON è quello del tag: schema visto $SCHEMA_VISTO, il tag dichiara $SCHEMA_ATTESO — il verdetto non vale (il registro: $LOG)" >&2
  exit 2
fi

# L'ESITO SI LEGGE dalla riga di RIEPILOGO dell'harness (corre un test
# solo), e quella riga va su STDOUT — nel file di cargo, `$JSON`: il
# registro `$LOG` è il suo stderr. (E con --nocapture l'output del test si
# infila in mezzo alla riga «test … FAILED»: quella non si può cercare.)
if [ "$st" -eq 0 ] && grep -q '^test result: ok\. 1 passed; 0 failed;' "$JSON"; then
  echo "NON cade sul tag $TAG: il difetto nasce da questa fase — si corregge (il registro: $LOG)"
  exit 0
fi
if grep -q '^test result: FAILED\. 0 passed; 1 failed;' "$JSON"; then
  echo "CADE ANCHE sul tag $TAG: il difetto c'era già — si dichiara aperto in open-defects.txt col seme e questo verdetto (il registro: $LOG)"
  grep -A 8 "^thread 'zz_seme_sul_tag'" "$LOG" | grep -v "^note: run with" | head -n 9
  exit 1
fi
echo "la corsa sul tag $TAG non è riuscita (compilazione o ambiente): guarda $LOG" >&2
tail -n 20 "$LOG" >&2
exit 2
