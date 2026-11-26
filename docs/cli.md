# `engramma` CLI reference (v0.1 + v0.2)

Common options (global):

| Option | Meaning |
|---|---|
| `--root DIR` | data root (default `~/.engramma`, or `$ENGRAMMA_ROOT`); cells live in `<root>/cells/<name>.sqlite` |
| `--cell NAME` | cell to operate on (default `default`); names are `[a-z0-9_-]+` |
| `--json` | stable JSON output (schemas below) |

**Exit codes**: `0` success, `2` usage error, `3` note not found, `4`
internal error. Note ids accept a unique prefix or suffix (≥ 4 characters).
Short ids shown by the CLI and used in export file names are the **last 8**
characters of the ULID — its random part: the first 8 are a timestamp and
collide in bulk imports.

## Glossary (v0.6.19)

From v0.6.19 everything a user, a client or a developer reads is in English:
the CLI's human output, the review page, the MCP server, the OAuth pages,
the refusals of sync, the Node binding's errors. The Italian documents
(`DESIGN.md`, `sync.md`, `node.md`, `mcp.md`) keep their own words: this
table is the bridge. `--json` did not change — who parsed the textual
output switches to it. The same words are the ones the beta printed in
Italian.

| The CLI says | The Italian docs say | What it is |
|---|---|---|
| note | nota | the unit of memory |
| cell | cella | one memory: `<root>/cells/<name>.sqlite` |
| queue | coda | where the assistants' proposals wait |
| proposal | proposta | what an assistant asks to store |
| decision | decisione | accept or reject, signed by who decides |
| correction | rettifica | a `supersede` proposal |
| revert | annullamento | undoing a decision, the `git revert` of the registry |
| tombstone | lapide | a deleted note: the text stays, search hides it |
| purge | oblio, purga | the right to be forgotten: the content goes |
| incomplete forgetting | oblio incompleto | a purge that could not reach every copy (exit 5) |
| quarantine | quarantena | written, not yet confirmed: search never shows it |
| conflict copy | copia di conflitto | the losing body of a concurrent edit, kept as a note |
| phantom | fantasma | a living row on a purged id |
| log | registro | the hash-chained operations log |
| snapshot | istantanea | the compaction's file copy of the cell |
| cut | taglio | where the compaction stopped keeping payloads |
| view | vista | a Markdown folder bound to the cell (`vault`) |
| embed | incorporare | give a note its vector |
| coverage | copertura | how many living notes have their vector |
| support | supporto | `strong` (text agrees) or `weak` (vector alone) |
| graph off | grafo spento | the question named no entity: the walk never starts |
| device | dispositivo | one of the paired copies |
| laptop | portatile | the default name of the first device |
| pairing | abbinamento | `invite`, `code`, `join` |
| verification code | codice di verifica | the six digits compared on both screens (SAS) |
| ticket | biglietto | the invite's text, `engramma-invite-v1.…` |
| code | codice | the invite's short form, `NUMBER-WORD-WORD` |
| mailbox | casella postale | the blind node that relays what devices miss |
| pass | passata | one round of synchronization |
| direct | diretta | a session that rode no relay; in JSON `via` is `"diretta"` or `"relay"` — stable values, Italian for good |
| epoch | epoca | the generation of a shared cell's key |
| frontier | frontiera | what every device acknowledged |
| connection | collegamento | an OAuth link between a client and the memory |
| request | richiesta | an authorization waiting for the human |
| label | etichetta | the actor of a connection's proposals, `mcp:<label>` |
| scopes | ambiti | `memory:read`, `memory:propose` |
| policy | politica | who may propose, how far the machine may go |
| reviewer | revisore | the automatic review of a proposal |
| breaker | interruptore | `max_auto_per_hour` tripped: a human decides |
| experimental | sperimentale | `--experimental` (the beta's `--sperimentale` still answers) |

## Commands

### `init`
Creates `<root>/cells/` and the `default` cell.
JSON: `{"root": "<path>"}`.

### `cell list|create|drop`
`list` JSON: `{"cells": ["default", ...]}`; `create`/`drop` JSON: `{"ok": "<name>"}`.

### `add FILE`
`FILE` is the body Markdown (`-` = stdin). Required: `--source SRC`,
`--confidence stated|inferred|external`. Options: `--type` (default `fact`),
`--title`, `--tag` (repeatable), `--valid-from DATE`, `--lang it|en|fr|de|es|pt`.
An `external` note is created `quarantined`.
JSON: `{"id": "<ulid>"}`.

### `get ID`
Human: the canonical note file (quarantined and deleted notes get a
`⚠ status: quarantined|deleted` warning line first — the CLI may show them, the MCP server may
not). JSON: the complete note object:
`{id, type, title, body, tags, lang, source, confidence, status, valid_from,
valid_to, supersedes, created_at, updated_at, x}`.

### `edit ID`
Opens the note in `$EDITOR`; on save the difference is applied as a
`patch_note` operation. The `id` and `created_at` cannot change. Clearing the
`lang` field is not expressible in v0.1 (omit it instead). JSON:
`{"id": ..., "patched": true}` or nothing changed → exit 0.

### `rm ID` / `confirm ID`
`rm` is a logical delete (tombstone; `verify` and the log keep it).
`confirm` promotes a quarantined note to active, recording the actor.
JSON: `{"ok": "<id>"}`.

### `supersede OLD FILE`
Closes `OLD` (validity end = the new note's `valid_from`, or today) and adds
the new note from `FILE`. Same required flags as `add`. JSON:
`{"closed": OLD, "new": NEW}`.

### `search TEXT`
Options: `--k` (default 10), `--hops 0-2` (default 1), `--as-of YYYY-MM-DD`,
`--tag` (repeatable, children included), `--type` (repeatable),
`--include-quarantined`, `--lang`.
JSON:

```json
{ "query": "...",
  "hits": [
    { "score": 0.94,
      "explain": { "matched_terms": ["trasloc"], "bm25_rank": 1,
                    "via": "[[Fabio]] -lives_in-> [[Milano]]",
                    "confidence": "stated", "valid_from": "2026-09-01",
                    "valid_to": null, "source": "user" },
      "note": { "id": "...", "type": "fact", "title": "...", "body": "...",
                 "tags": [], "source": "user", "confidence": "stated",
                 "status": "active", "valid_from": "2026-09-01",
                 "valid_to": null, "created_at": "...", "updated_at": "..." } }
  ] }
```

Human output prints score, matched terms, BM25 rank, graph path, validity and
source for each hit. Quarantined and tombstoned notes never appear; superseded
notes appear only for `--as_of` questions they answer.

### `neighbors ENTITY [--depth N] [--rel R ...] [--include-quarantined]`
Edges declared by quarantined or deleted notes are excluded (manual `link`
edges always stay); `--include-quarantined` brings the quarantined ones back.
JSON: `{entity, neighbors: [{depth, entity, rel, direction, note_id,
valid_from, valid_to}]}`.

### `path A B [--include-quarantined]`
Shortest graph path (≤ 4 steps), same edge visibility rules as `neighbors`.
JSON: `{from, to, steps: [{from, rel, to}]}` or `steps: null`.

### `tags [PREFIX] [--include-quarantined]` / `entities [PREFIX]`
A tag prefix includes its children; quarantined and deleted notes do not count
unless `--include-quarantined` (deleted never). JSON: `{tags: [{name, parent,
note_count}]}` / `{entities: [{name, id, note_id}]}`.

### `timeline ENTITY [--include-quarantined]`
Notes about an entity ordered by validity (oldest first), with each note's
explain fields.

### `history ID` / `restore ID VERSION`
`history` JSON: `{id, versions: [{version, op_id, note}]}`. `restore` is
itself an operation in the log. JSON: `{"ok": "<id>"}`.
**(v0.6.14, N2) `restore` brings back the CONTENT of the version, not its
state**: body, title, tags, type, scope, language, validity and
extensions come back; status, confidence, `supersedes`, source and the
birth date stay as the operations of the lifecycle left them (DESIGN §
4.4 has the two lists). A closed note stays closed — keeping its closing
date — a note in quarantine stays there, a note the user confirmed is
not demoted. The way back from a tombstone is `status` (or a patch whose
base is the tombstone), no longer `restore`.

### `import DIR [--obsidian]`
Engramma folder (with `engramma.json`) or, with `--obsidian`, any Markdown
vault: hidden entries (`.obsidian/`, `.trash/`) and non-Markdown files are
skipped, symlinks never followed, notes without an Engramma header get one
(`source: import:obsidian/<relative path>`). JSON:
`{imported, skipped: [..], skippedPurged: [id..], errors: [..], warnings: [..]}`.
`skippedPurged` (v0.6.9) nomina gli id che la cella ha purgato: il file
torna, l'id no — l'import non si ferma, e tocca all'utente buttare o
riscrivere quei file con un id nuovo (`docs/node.md` dice come li legge
Homun).

### `export DIR`
Writes `<dir>/<type>/<slug>-<id8>.md` plus `engramma.json`. Tombstones are
never exported; superseded notes are.

### `verify`
Recomputes the hash chain, rebuilds the whole state by replaying the log in
memory, compares every table and runs the FTS integrity-check (the index
must hold exactly the non-tombstoned notes). After a compaction the `replay`
field (and a human line) says what was actually checked: `full` (no
compaction), `from_snapshot` (the tail replays on the snapshot file) or
`chain_only` — a checkpoint without a file, where verify checks the whole
chain, every surviving payload's digest, the derived indexes (the `d…`
edges re-derived from the notes, no vector for a note that no longer lives)
and the state digest while the head has not moved past the checkpoint.
**(v0.6.14, N1.2)** verify also checks the compaction threshold: every
operation whose payload the cut removed sits at or below the registered
cut — one above it is a broken cell (a cut that went back, as engines
before v0.6.14 accepted), and `mismatch` names it.
JSON: `{ops, replay, hash_chain_ok, replay_ok, fts_ok, mismatch,
phantom_purged, declared_corners, coverage, ok}` — `declared_corners`
(v0.6.14, N1.3) is prose, never a failure: the corners a migration
declared (today one kind: a cut that went back under the v0.6.13 engine,
with no copy file to rebuild some objects from); the human output prints
one line each — `coverage` lists the vector coverage per
model (`{model, state, covered, living}`; embeddings are derived data,
their absence is never a verification failure). The human output prints one
`vector coverage` line per model. **(v0.6.10, J2) verify is read-only**:
it runs inside a deferred transaction that always rolls back, and when the
cell holds «phantom» notes — a living row on a purged id, written by the
v0.6.7/v0.6.8 engines — it NAMES them (`phantom_purged`), the outcome says
the cell needs repair, and not one byte moves. The phantom is content the
user wrote after the purge (a reimport, a backup): the rule forgot the id,
not the text.

### `repair` (v0.6.10)
The explicit step verify asks for. For each phantom, in its own
transaction (an interruption leaves every note either as it was or
repaired, never halfway): the content of the living row goes to a note
with a NEW id, with a normal `put` (actor `engramma:repair`); then the
forgetting rule applies to the old id — state rows, closure over the
conflict copies, log scrub, `secure_delete`. The same call settles the
rows the v0.6.9-or-older engines wrote with halves that no longer replay
(the resurrected `supersedes` of a decide over a purged proposal): the
co-objects re-derive, and verify is green after. JSON:
`{moved: [{old, new}], verify_ok}` — the map goes to the CALLER (Homun's
references) and is saved nowhere: the forgotten id must not stay written
anywhere new. The human output prints `id8 → id8` per note.

### `--version` (v0.6.10)
Prints the version, the **commit** the binary was built from (with the
`-dirty` mark when the tree was not clean) and the cell **schema** it
supports: `engramma 0.1.0-dev (commit 97669d2, schema 22)`. The JS bench
compares this line with the addon's own build and refuses to run when
they disagree — a test that passes or falls according to which binary
lies around in `target/` measures nothing.

### `search TEXT` (v0.2)
Options: `--mode text|vector|hybrid` (default: `hybrid` when the cell has a
`ready` embedding model matching the configured embedder, else `text`),
`--fusion rrf|convex-0.3|convex-0.5|convex-0.7` (a `convex-mm-`/`convex-th-`
prefix picks the cosine normalisation: min-max over the candidates, or the
theoretical −1..1 limits),
`--tau` (abstention floor for vector-only results, default 0.30),
`--strict` (drop `support: "weak"` results: vector-only with a thin
cosine margin — near in meaning, unconfirmed by any discriminative text
match), plus the v0.1 options (`--k`, `--hops`, `--as-of`, `--tag`,
`--type`, `--include-quarantined`, `--lang`).

JSON adds `mode`, `embed_ms`, `search_ms`, and per hit: `raw_score` (before
the display normalisation — the top hit is not always "1.0 of something")
and the new explain fields (`support` is `"strong"`/`"weak"`):

```json
{ "query": "...", "mode": "hybrid", "embed_ms": 8.2, "search_ms": 3.1,
  "hits": [
    { "score": 1.0, "raw_score": 0.612,
      "explain": { "matched_terms": ["milan"], "bm25_rank": 1,
                    "semantic_rank": 2, "cosine": 0.8268,
                    "fused_score": 0.93, "lists": ["text", "vector"],
                    "winning_chunk": 0, "via": null,
                    "confidence": "stated", "valid_from": "2026-09-01",
                    "valid_to": null, "source": "user" },
      "note": { ... } } ] }
```

Human output prints lists, ranks, cosine and the winning chunk.

### `model list|pull ID|use ID` (v0.2)
`list` shows the known models (builtin + user folders with their own
`manifest.json`), their size, whether they are downloaded, and their state
in this cell (`building|ready|retired|—`). `pull` downloads the pinned
revision and verifies each file's sha256 before renaming it into place (a
failed check leaves no partial file). `use` registers the model in the
cell as `building` — the blue/green switch — and writes `<root>/config.json`
for the builtin kind. Remote embedders are configured by writing the file
by hand:

```json
{ "embedder": { "kind": "ollama", "model": "nomic-embed-text",
                 "url": "http://127.0.0.1:11434", "dim": 768 } }
{ "embedder": { "kind": "http", "model": "text-embedding-3-small",
                 "url": "https://api.example.com", "token_env": "MY_TOKEN",
                 "dim": 1536 } }
```

JSON `list`: `{model_root, models: [{id, model_id, dim, max_tokens, bytes,
downloaded, state}]}`; `use`: `{model, state: "building", config}`.

### `embed [--all]` (v0.2)
Embeds the pending notes with the configured embedder, in batches, and
promotes a `building` model to `ready` when its coverage reaches 100% (the
old model retires and its vectors are deleted, in one transaction).
`--all` re-embeds every living note. JSON: `{embedded, pending, promoted?}`.
`add` and `import` run the pass automatically (`--no-embed` skips it: the
notes stay pending).

### `mcp --cells a,b [--write a --actor name]`
Starts the read-only MCP server on stdio (the HTTP transport is a v0.5.2
flag of the same command, see below). Without `--cells` it refuses to
start (no cell would be exposed). `memory_search` joins the vector branch
automatically when the cell has a `ready` model matching the embedder in
`<root>/config.json`; without a configuration every search is the v0.1
text+graph one.

### Models: where they live, and licences

`ENGRAMMA_MODEL_DIR` overrides the model root (default
`~/.cache/engramma/models`). The builtin Granite models are Apache-2.0.
EmbeddingGemma is supported only as a user folder with its own manifest
(its Gemma licence forbids bundling or auto-downloading the weights); no
test runs with it.

## v0.3 commands

### `engramma purge <id> --reason <why> [--yes]`

The right to be forgotten: the note, its versions, the edges it declares,
its vectors, its FTS row, the entities left with no references and the
tags no note carries anymore — all gone, plus the **payloads of its
operations** (log v2: the chain covers salted digests, not the bodies).
`secure_delete` zeroes the freed pages, the FTS `optimize` merges the
segments (the **stems** leave the index too) and a `VACUUM` rebuilds the
file: no byte — and no root — of the body survives in the cell. The
purge's own payload keeps only the id and the reason, never content.
`[[Mentions]]` of the note in OTHER notes stay (they are other notes).
**Exports made before the purge are outside the engine's control.**

v0.3.2: the cell remembers its views and the purge reaches every one of
them right away — the note's file AND every `.conflitto` copy of it go in
the same command (`Vault::forget_note`; the MCP server is read-only and
does not expose purge, but the call is there for when it does). A
`delete_note` (tombstone, reversible) does NOT remove conflict copies.

v0.4: a view has an **identity**, not a path. `vault init` writes a
`view_id` (ULID) into the folder's `engramma.json`; the cell binds its
`vault_views` row (and the view's file and conflict-copy rows) to that
id. Moving the folder and syncing from the new place UPDATES the root —
one view, not two — and a purge reaches the view at its new place. An
unreachable view keeps the purge pending (exit **5**,
`pending: ["vault"]`) **only while it still holds something of the note**
(a bound file or a conflict copy); the message says a view was
unreachable and offers both ways out — `vault sync` to reach it again,
`vault unbind` to detach it. A cell whose views cannot even be read is not
a completed forgetting either.

The WAL is truncated after the vacuum (`wal_checkpoint(TRUNCATE)`). When
another process holds a read transaction open, the truncation cannot
happen: the purge says so and exits with code **5** (`scrubbed: false`,
`pending: ["wal"]`); the checkpoint is retried on the next write and at
cell close, and `--json` carries the same fields. A failed `VACUUM` or
PRAGMA is an error, never a silent success.

### `engramma compact [--experimental] [--keep-last N] [--keep-snapshot]`

**(v0.6.15, F2) Sperimentale, dietro un interruttore esplicito.** Senza
`--experimental` (v0.6.19: la beta lo chiamava `--sperimentale`, che
risponde ancora) il comando rifiuta (exit 2) e dice perché: i difetti
dichiarati del taglio sono aperti
(`crates/engramma-core/tests/open-defects.txt`). La compattazione è
un'ottimizzazione — la manutenzione di una memoria che cresce — e finché
i suoi difetti sono aperti sta all'utente deciderla, non al motore. Il
binding JS chiede la stessa cosa all'apertura della cella:
`{ experimental: ['compaction'] }` (docs/node.md).

Registers a **checkpoint** and forgets the payloads up to `head − N`
(default `keep-last` 1000): the row records the head hash and the state
digest of the logical state at the head, **without a file** — the live
cell already is the durable state, and a second copy of it (the v0.3.1
snapshot) only served the full audit. The skeletons and the hash chain
stay whole; the last `keep-last` operations stay replayable;
`changes_since` below the cut answers `Compacted`. `verify` says what it
could check (`replay: chain only from seq N (compacted without a
snapshot)` in `replay`): the whole chain, every surviving payload's
digest, the derived indexes — but no from-zero replay is possible.

With `--keep-snapshot` the v0.3.1 behavior: `VACUUM INTO
<cell>-snap-<seq>.sqlite`, stripped to the **logical state** (payloads
up to the cut, vectors, model registry, vault bindings and FTS content
go). Only the **last** row stays, with or without a file: the latest
compaction replaces the previous one, and a compaction without the flag
deletes the file of the previous one. A later `purge` scrubs a snapshot
file (its row records the redacted digest with `redacted_by` naming the
purge, spec §6.4) and **nulls a checkpoint's digest** (it described a
state that contained the purged note).

**(v0.6.14, N1) The cut only goes forward.** A compaction asking a cut
BELOW the one already made does nothing: it prints
`no new cut: the cut stays at seq N` (JSON `"compacted": false`
with the true `cut_seq`) — not an error, so an automatic caller that
raises `keep-last` never fails. The same cut asked again keeps the
behavior it always had: the restart point is registered again, in the
form asked (a file snapshot becomes a checkpoint, and the other way
round).

`--json` carries `cut_seq`, `head_seq`, `head_hash`, `state_digest`,
`snapshot` (null for a checkpoint), `bytes_before`/`bytes_after` — the
latter measured over cell + WAL + snapshot files.

### `engramma vault init <dir> | sync [dir] [--allow-deletions] | watch [dir] | unbind <dir|view_id>`

The markdown view: `init` exports the cell into the folder; `sync` runs
one scan-based reconciliation and prints the report (new notes from
files, notes updated from files, files rewritten from the cell,
conflicts — the cell wins and the file's version becomes a
`<name>.conflitto-<ts>.md` copy — deletions, moves); `watch` reconciles
on filesystem events (500 ms debounce) and every 60 s, Ctrl-C exits. A
file is bound to its note by the `id` in the header, never by its path:
renaming or moving a file moves the note's binding. More than 20 files
(or 10% of the notes) vanishing at once stops the sync with an error —
an unmounted folder — unless `--allow-deletions`.

`unbind` (v0.4) detaches a view: the cell forgets it and ITS bindings
(files and conflict copies); the files stay on disk, they were never the
engine's. Accepts the folder or the view id — full, or its last 8
characters.

### `engramma consolidate [--limit N] [--json]`

Near duplicates (cosine ≥ 0.95 on the ready model AND entity Jaccard
≥ 0.5) and probable contradictions (same subject, same functional
relation — `lives_in`, `works_at`, `partner_of` — two destinations,
overlapping validity). Proposals only: the engine never merges. Not
exposed over MCP.

## v0.5 commands

### `engramma proposals [show|accept|reject|review]`

The queue of what the assistants proposed, and its decisions. Without a
subcommand: the queue (and the decided ones with `--json`). `show <id>`
is one proposal in full — note, proposer, decision, reviewer metadata.
`accept <id>` makes the note active and computes its vector right away
(v0.6.18, like `add` does; `--no-embed` skips it — the note waits for
`engramma embed`), `reject <id>` refuses it (the tombstone keeps the text,
the revert can undo), `review` retries the FAILED automatic reviews
(never the escalated ones) and embeds what it makes active.

### `engramma decisions [--by ACTOR] [--proposed-by ACTOR] [--since DATE] [--until DATE] [--state active|reverted]`

The decisions registry: who, when, what, why. `--by` takes an exact
actor or a prefix (e.g. `reviewer:`); dates are `YYYY-MM-DD`, `--until`
inclusive. `--proposed-by` (v0.6.18) narrows to the decisions taken on
the proposals of that actor (exact or prefix, e.g. `mcp:claude-code`):
look before you undo.

### `engramma revert [DECISION] [--by ACTOR] [--proposed-by ACTOR] [--since DATE]`

Cancel decisions — the git revert of the registry. By decision op id
(or its last-8 prefix), or in bulk with `--by` (every decision of that
actor, prefix) narrowed by `--since`.

Who DECIDES and who PROPOSES are two different people: a decision is
signed by its decider (`user@cli`, `reviewer:<name>`), and an `mcp:`
actor only ever proposes — `--by mcp:…` would select nothing, so it is
a usage error (v0.6.18). To undo what an assistant filled the queue
with, `--proposed-by mcp:claude-code` selects every decision taken on
that actor's proposals (exact or prefix), combining with `--by`,
`--since` and `--until`; `--dry-run` previews, the `[y/N]` prompt
confirms (`--yes` skips it). The decisions whose content was purged
keep counting among the `purged` — cancelled in the log, nothing comes
back.

A decision another correction stands on is not reverted before that
one (v0.6.18), like a commit in git: if C corrects the note P made
current, reverting P's decision is a conflict that names C — revert C
first. In bulk the run goes newest first, so a filter that selects
both undoes both, and `--dry-run` says so.

### `engramma policy show|set`

The cell's policy: who may propose, how far the machine may go.
`set write=… reviewer=… min_confidence=… kinds=a,b
allow_supersede=… max_auto_per_hour=…` — only from here, never from
the web, never from MCP.

### `engramma review [--port N] [--open]`

The local review page: the queue, the decisions, the notes. Serves on
127.0.0.1 only (any free port by default), `--open` opens the browser
at the printed address, Ctrl-C stops it.

## v0.5.2 commands

### `mcp --http 127.0.0.1:8787 --public-url https://memoria.example.net [--cells a,b] [--write a] [--allow-dcr] [--compat chatgpt]`

The HTTP transport (see `docs/http.md`): the loopback listen address is
mandatory and only loopback is accepted; `--public-url` (https, no query,
no fragment) is the address the tunnel publishes and every OAuth URL is
derived from it. `--write` names the cells that accept proposals (a
subset of `--cells`) — but there is **no `--actor`** here: over HTTP the
proposing actor is the grant's label (`mcp:<label>`), chosen when you
approve the connection. `--allow-dcr` turns on RFC 7591 dynamic
registration (deprecated by the spec; off by default), `--compat chatgpt`
adds the fixed-schema `search`/`fetch` wrappers. Every request to `/mcp`
wants a bearer token; the access log on stderr carries time, grant id,
MCP method, tool, status and duration — never tokens or note text.
SIGINT/SIGTERM cancel the transport, drain the listener and close the
cells.

### `engramma oauth pending | approve | deny | grants | revoke | log`

The human surface of the HTTP authorization flow (state in
`<root>/oauth.db`, 0600):

- `pending` — the requests waiting for you: code, client, redirect host,
  scopes, age.
- `approve <XXXX-XXXX> --label chatgpt [--cells a] [--scope memory:read]
  [--yes]` — the code is the one the **user's browser** shows. The label
  (`[a-z0-9-]{1,32}`) becomes the actor `mcp:<label>` of the proposals;
  `--cells`/`--scope` only restrict what the server offers (a cell the
  server does not offer is a usage error). Before the confirmation the CLI
  prints the client's host and the redirect's. The code may be typed in
  lowercase and without the dash. A label belongs to one **live**
  connection at a time: after `revoke` (or the 90-day expiry) the same
  label can be approved again for the reconnection.
- `deny <XXXX-XXXX> | --all` — refuse it, or every pending request at
  once (the flood answer: the waiting pages redirect each client away
  with `access_denied`).
- `grants` — label, client, cells, scopes, creation, last use, standing.
- `revoke <label> | --all` — the tokens die at their next request (by
  label: the live grant under that label).
- `log` — the event history: approvals, refusals, revocations, reuse
  alarms, rejected clients. No secrets, no note text.


## `engramma devices` (v0.6.1)

- `init` — crea `device.key` (o mostra l'esistente) e il primo dispositivo dell'elenco
- `invite [--name <name>] [--relay <url>|--no-relay]` — lega l'endpoint, stampa il biglietto con l'indirizzo vero e resta in attesa (10 minuti, un uso); SPAKE2 sul segreto, conferma di chiave RFC 9382, sei cifre su entrambi gli schermi. `--name` (predefinito `laptop`) nomina questo dispositivo quando l'elenco nasce qui (v0.6.2)
- `code [--name <name>] [--relay <url>|--no-relay]` — apre il canale sul nodo cieco e stampa il codice breve (`7-tigre-lago`); dopo le sei cifre un biglietto fresco passa sigillato nel relay e l'ingresso corre in diretta. `--name` come sopra (v0.6.2)
- `join <ticket-or-code> --name <name> [--relay <url>|--no-relay]` — l'ingresso (biglietto in diretta, codice tramite il nodo); le sei cifre si confermano su entrambi
- `list` — nome, id corto, chiave di trasporto
- `remove <name>` — la `remove` firmata con la radice e il conto dell'insieme accettato, l'epoca nuova di ogni cella condivisa, le chiavi avvolte per i rimasti; tutto raggiunge gli altri alla sessione successiva. Ciò che il rimosso aveva lo tiene
- `log` — il registro dell'elenco, con le firme

## `engramma sync` (v0.6.1)

Un solo servizio per radice (v0.6.2): un lucchetto esclusivo (`<radice>/sync.lock`) copre `serve` e ogni `run`; un secondo rifiuta con una riga dedicata e uscita 7.


- `share <cell>` — `adopt` + epoca 0; dopo la conferma la cella viaggia verso tutti i dispositivi
- `unshare <cell>` — smette da qui; le copie altrove restano (e lo dice)
- `run [--relay <url>|--no-relay]` — una passata con ogni dispositivo vivo raggiungibile e il nodo cieco configurato, poi esce; il riepilogo JSON dice inviate/ricevute/rifiuti/conflitti/frontiera per cella, e per dispositivo se la sessione è avvenuta e per quale strada (`diretta` o `relay` in JSON — valori stabili, restano italiani; sulla riga per umani `direct`) — un irraggiungibile è una riga, non un errore
- `serve [--relay <url>|--no-relay]` — in ascolto (ALPN `engramma/sync/3`), cancello all'accettazione, sincronizza a ogni scrittura locale (rimbalzo 2 s) e ogni 5 minuti; stop pulito su SIGINT/SIGTERM; convive con un altro processo che scrive sulla stessa radice. La scelta del relay si salva nella radice: la si dà una volta, non a ogni avvio (un flag esplicito vince)
- `status` — per cella: dispositivi, ultima sessione, operazioni in attesa, frontiera stabile, epoca

## `engramma conflicts` (v0.6.4)

The conflict copies to resolve: who lost, when, in quarantine or not —
the copy is a note, read it with `get`.

- `show <cell> <copy>` (v0.6.5) — una copia per intero: il corpo
  dell'originale, il corpo alla biforcazione e il corpo della copia —
  anche per una copia in quarantena, che `get` non mostra mai; una copia
  inesistente o chiusa risponde «is not an open conflict copy»
- `resolve <copy> --keep original|copy|merged [--body-file <f>]` — la
  risoluzione: `original` tiene l'originale (la copia va via), `copy` fa
  vincere il corpo della copia, `merged` prende la fusione a mano da un
  file (`-` = stdin)

## `engramma mailbox` (v0.6.1)

- `serve --root <dir> [--relay <url>|--no-relay]` — il nodo cieco in esecuzione (ALPN `engramma/mailbox/3`, ritenzione 180 giorni): stampa il proprio biglietto, deposito/enumerazione/consegna, purge firmata, incontro dei codici
- `add <node-ticket>` — registra il nodo e gli consegna l'elenco firmato
- `status` — ultimo contatto, buste conservate (conteggio e byte, chiesti al nodo), ritenzione
- `remove` — la radice lo dimentica

Ogni comando accetta `--json`. Gli errori non contengono testo di note.
