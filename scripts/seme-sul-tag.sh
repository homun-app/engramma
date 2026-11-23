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
# Come lavora: un worktree del tag DENTRO target/ (mai in $TMPDIR), con
# la stessa CARGO_TARGET_DIR del repo (le dipendenze sono già costruite);
# nel file di test della proprietà, nel worktree, un test che chiama il
# corpo della proprietà col seme; una corsa; il verdetto; il worktree se
# ne va da solo alla fine, anche se il test fallisce. Il repo non cambia:
# il test vive nel worktree e muore con lui.
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
#       2 = proprietà, seme o tag sbagliati; 3 = il tag non ha il corpo.

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

# il worktree: DENTRO target/, a un percorso fisso per tag (così cargo
# riusa quel che ha costruito alla corsa prima)
WT="$REPO/target/seme-sul-tag/$TAG"
mkdir -p "$(dirname "$WT")"
LOGS="$REPO/target/seme-sul-tag/log"
mkdir -p "$LOGS"
LOG="$LOGS/$PROPRIETA-$SEME-su-$TAG.log"

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

# il test: chiama il corpo col seme. Un corpo può rispondere () (fallisce
# con un panic) o Result (fallisce con Err): l'esito si legge uguale.
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
    if let Err(messaggio) = $CORPO($SEME).esito() {
        panic!("il seme cade: {messaggio}");
    }
}
EOF

pacchetto=$(basename "$(dirname "$(dirname "$FILE")")")
bersaglio=$(basename "$FILE" .rs)

echo "«${PROPRIETA}», seme $SEME, sul tag $TAG ($TAG → $(git -C "$REPO" rev-parse --short "$TAG^{commit}")), corpo $CORPO"
(
  cd "$WT" || exit 2
  CARGO_TARGET_DIR="$REPO/target" CARGO_BUILD_JOBS=2 RUST_TEST_THREADS=2 \
    PROPTEST_DISABLE_FAILURE_PERSISTENCE=1 \
    nice -n 19 cargo test -p "$pacchetto" --test "$bersaglio" zz_seme_sul_tag
) >"$LOG" 2>&1
st=$?

if [ "$st" -eq 0 ]; then
  echo "NON cade sul tag $TAG: il difetto nasce da questa fase — si corregge (il registro: $LOG)"
  exit 0
fi
if grep -q "^test zz_seme_sul_tag \.\.\. FAILED" "$LOG"; then
  echo "CADE ANCHE sul tag $TAG: il difetto c'era già — si dichiara aperto in open-defects.txt col seme e questo verdetto (il registro: $LOG)"
  grep -A 12 "^---- zz_seme_sul_tag" "$LOG" | head -n 14
  exit 1
fi
echo "la corsa sul tag $TAG non è riuscita (compilazione o ambiente): guarda $LOG" >&2
tail -n 20 "$LOG" >&2
exit 2
