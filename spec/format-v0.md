# Engramma note format — specification v0

Status: **draft, unstable** (v0.1). Fields are in English; content is in the user's
language. This document is normative for `engramma-core::format` in v0.1.

Embedding vectors are **not** part of this format: they are derived data
recomputed from the notes (see `docs/DESIGN.md` §6.2) and never travel in
note files or exports.

A note is a UTF-8 Markdown file with an optional YAML frontmatter header:

```text
---\n
<YAML mapping, one document>
---\n
<body: verbatim bytes to end of file>
```

Rules:

- The two delimiter lines are exactly `---` followed by `\n` (LF). CRLF files
  are invalid in v0.1.
- A file with no frontmatter is not a valid Engramma note (the importer gives it a
  header; see §7).
- The body is preserved **verbatim**: no trailing-newline normalisation, no
  re-wrapping. `parse` keeps it as-is.
- The YAML mapping must have string keys. Complex keys and explicit tags are
  rejected; anchors and aliases are resolved by the parser (the canonical
  emitter never writes them).

## 1. Standard fields

| Field | Type | Required | Allowed values / format |
|---|---|---|---|
| `id` | string | yes | ULID (26 Crockford base32 chars, canonical uppercase) |
| `type` | string | yes | `fact`, `person`, `place`, `project`, `event`, `preference`, `note` |
| `title` | string | no | any single-line text; if absent the first non-empty body line is the display title |
| `tags` | list of strings | no | each matches `tag-name` below |
| `scope` | string | no | logical scope inside the installation |
| `lang` | string | no | stemming language for the text index: `it`, `en`, `fr`, `de`, `es`, `pt`; overrides detection |
| `source` | string | yes | provenance, e.g. `user`, `homun:chat/bot-casa/8812`, `import:obsidian/...` |
| `confidence` | string | yes | `stated`, `inferred`, `external` |
| `status` | string | no | `active`, `quarantined`, `superseded`, `deleted`; default `active` |
| `valid_from` | date or datetime | no | ISO 8601; date `YYYY-MM-DD` or datetime `YYYY-MM-DDTHH:MM:SSZ` (UTC only) |
| `valid_to` | date or datetime | no | same as `valid_from`; absent = still valid |
| `supersedes` | list of strings | no | ULIDs of notes replaced by this one |
| `created_at` | datetime | yes | `YYYY-MM-DDTHH:MM:SSZ` (UTC only, seconds precision) |
| `updated_at` | datetime | yes | same format |
| `x-*` | any | no | free extensions; keys must start with `x-`; preserved in file order |

Validation rules (each violation is a readable error):

1. `id`, `type`, `source`, `confidence`, `created_at`, `updated_at` are required.
2. A date field accepts `YYYY-MM-DD` or a UTC datetime; the `*_at` fields accept
   **only** a UTC datetime. On round-trip `valid_*` keeps the granularity it was
   written with; `*_at` always serialises as `YYYY-MM-DDTHH:MM:SSZ`.
3. `confidence: external` notes are **born** `quarantined` (the cell API
   enforces this at creation); after `confirm` they are `active`, and later
   they can be `superseded` — so a file may legitimately carry `external` with
   any status, and only the cell knows whether the lifecycle was followed.
4. `status: deleted`/`superseded` is valid on parse (exports carry them) but the
   cell API never writes them directly through `put_note`.
5. Unknown keys:
   - starting with `x-`: preserved, in file order, after the standard keys;
   - not starting with `x-`: **error** in strict mode (the default for Engramma
     folders); **warning + dropped** in import mode (Obsidian and friends).
6. Duplicate keys are a YAML error (saphyr rejects them).
7. `tags` entries must match `tag-name`: one or more segments separated by
   `/`; each segment starts with a letter, digit or `_` (Unicode letters
   allowed) and continues with letters, digits, `_` or `-`. No empty segment.
   `casa/milano` declares `casa` as parent of `casa/milano`.
8. `status: superseded` requires `valid_to`: the replaced fact must say when
   it stopped being true, or two "current" facts about the same subject would
   coexist. Strict mode makes it an error; import mode a warning — the
   importer then closes the validity at the `valid_from` of the note that
   cites it in `supersedes`, when there is one.

## 2. Body: links

`[[Name]]` and `[[Name|text]]` inside the body create a `mentions` edge from the
note to the entity `Name` (the entity is created if missing; names are
case-insensitive, first-seen spelling is canonical). Links are recognised in
**text and heading text**, and are **ignored inside fenced/indented code blocks
and inline code** (CommonMark events, not a scan of the raw text). `[[Name|text]]`
links to `Name`; `text` is alias prose. `Name` must not be empty or contain
`[`/`]|`.

## 3. Body: relations

A `## Relations` section (any heading level 1–2 whose text is exactly `Relations`)
holds one relation per list item:

```text
- lives_in:: [[Milano]] (from 2026-09-01)
- partner_of:: [[Giulia]]
- worked_at:: [[Acme]] (from 2020-01-06 to 2024-12-31)
```

- Relation name: `[a-z][a-z0-9_]*` (lowercase; underscores; Dataview-compatible).
- The `(from DATE to DATE)` trailer is optional; either bound may appear alone.
  Dates follow the `valid_*` format. They override the validity inherited from
  the declaring note.
- The relation's **subject** is the note's *subject entity*; otherwise the note
  itself (`src` endpoint `note:<id>`). The subject entity exists when the note
  `type` is `person`, `place`, `project`, `event` or `preference` **and** the
  note title is a plain name: in that case an entity with that name (the title,
  case-insensitively) is the subject. Every other case uses `note:<id>`.
- Links inside the Relations section do **not** create `mentions` edges.

## 4. Body: tags

`#tag/child` in body **text** (not in headings, not in code) adds the tag to the
note's tags, exactly like a frontmatter tag. Tag characters as in §1.7 (hyphens allowed after the first character). A tag
must be preceded by start-of-text or whitespace and not be part of a `[[…]]`
link.

## 5. Canonical serialisation

`serialize(note)` is fully specified so that two implementations byte-agree:

1. Delimiter `---\n`, then standard keys **in this order**, each omitted when
   absent: `id`, `type`, `title`, `tags`, `scope`, `lang`, `source`,
   `confidence`, `status`, `valid_from`, `valid_to`, `supersedes`, `created_at`,
   `updated_at`. `status` is always written (even `active`) to keep exports
   explicit.
2. Then `x-*` keys in their original order.
3. Lists are flow: `[a, b]` after a tag name or a comma+space; empty list `[]`.
4. Strings are emitted bare when they contain no `": "` sequence, no trailing
   `:`, no `" #"` sequence, no leading `#` or whitespace, none of
   `{}[],&*?|<>=!%@\`` (and no tab/newline), are non-empty, and do not parse
   as a bool, null or number; otherwise single-quoted with `''` escaping
   (single and double quotes do not force quoting: they escape or pass
   through).
5. `x-*` values are emitted in flow style; maps `{k: v, k2: v2}` (insertion
   order), nested values by the same rules; null as `null`.
6. `valid_*` serialise as written (date stays a date); `*_at` as
   `YYYY-MM-DDTHH:MM:SSZ`.
7. Closing `---\n`, then the body verbatim (no added trailing newline).

Round-trip properties (both tested with proptest):

- `parse(serialize(n)) == n` for every note `n` the library can represent;
- `serialize(parse(x))` is stable (applying it twice changes nothing).

## 6. Change log: operation and hash rules (v2, forgettable payloads)

Every cell mutation is one row in `ops`. The row carries `seq` (insert order),
`op_id` (ULID), `hlc`, `actor`, `kind`, `note_id` and `payload_digest`. The
payload **body** lives in a separate table, `op_payloads(op_id PRIMARY KEY,
salt BLOB(16), body TEXT)`, and can be **forgotten** without breaking the
chain. The canonical JSON has **object keys sorted lexicographically at every
level, no whitespace** (serde_json with BTreeMaps; floats are emitted in Rust's
shortest round-trip form and are therefore avoided in payloads — timestamps and
dates are strings).

Hash rules (log v2):

```text
payload_digest = blake3(salt ‖ canonical JSON of the body)
hash           = blake3(prev_hash ‖ canonical JSON of
                        {actor, hlc, kind, note_id, op_id, payload_digest})
```

`prev_hash` is the `hash` of the previous op (ASCII lowercase hex, 64 chars)
or 64 `0` chars for the first op. The columns of `ops` ARE the envelope: the
hash binds them directly. `verify_log` recomputes the whole chain and, when
the payload is present, checks that `payload_digest` still matches
`blake3(salt ‖ body)` — a changed byte, or a changed salt, breaks it.

The **salt** is 16 random bytes generated when the operation is **created**,
like the op id and the HLC: `apply` stays deterministic (no clock, no
randomness, no I/O). A naked hash of personal data can still be personal
data; with the salt destroyed it cannot be linked back — the SD-JWT
technique (RFC 9901).

### 6.1 Golden vectors

The v2 rule (salt = 16 × 0x07, body `{"note":{"body":"corpo","x":1}}`,
op_id and note_id `01F7ZQ6VTSATW2BQ5Z9R0P1S8T`,
hlc `01a17720ca0000000007.0123456789ab`, actor `engramma`):

```text
payload_digest = aeff683a134c22f54b920c36db80d1f730083f862962decd73876ac533fb34d3
envelope       = {"actor":"engramma","hlc":"01a17720ca0000000007.0123456789ab",
                  "kind":"put_note","note_id":"01F7ZQ6VTSATW2BQ5Z9R0P1S8T",
                  "op_id":"01F7ZQ6VTSATW2BQ5Z9R0P1S8T",
                  "payload_digest":"aeff683a134c22f54b920c36db80d1f730083f862962decd73876ac533fb34d3"}
hash (genesis prev) = 13c81593af51e2faf2f301fdf1a2c85172048bc25c5cec2bb12411e974849418
```

The v1 rule (`hash = blake3(prev_hash ‖ payload_json_canonical)`, the
envelope inside the payload) stays frozen for the migration, whose golden
vector is

```text
prev_hash = 0000000000000000000000000000000000000000000000000000000000000000
payload   = {"actor":"engramma","hlc":"01a17720ca0000000007.0123456789ab",
             "kind":"init","note_id":null,"op_id":"01F7ZQ6VTSATW2BQ5Z9R0P1S8T","x":1}
hash      = a217239290ba91e17196ab1812bd01a1e3d05e71f6048387017212636c555d46
```

### 6.2 Redaction

A payload can be forgotten (`purge`, compaction): the `op_payloads` row goes,
the skeleton and the chain stay. Replaying a redacted operation is a **no-op**
— only its row and the HLC tick are replayed. This is only ever correct for
notes removed by `purge` or for ops under a compaction snapshot; in both
cases the live state agrees with the redacted replay. `purge` additionally
**censors** the supersede ops where its note was the NEW note: the body keeps
`old_id`, `valid_to` and `old_updated_at` with `note: null` (fresh salt, the
chain tail recomputed), so the OLD note's closure stays replayable; a
supersede whose OLD note is absent (its ops were purged) creates the new note
without the closure and without the `supersedes` row — exactly what the live
state looks like after that purge.

### 6.3 Migration v1 → v2 (schema v5)

On opening a v0.2.1 cell: the v1 chain is verified first (a broken log is
never rewritten), every op is rewritten in the v2 form with fresh salts, and
`meta` gains `log_v1_head` (the last v1 hash) and `log_v2_from_seq` (= 1).
The format of the log is declared unstable in 0.x.

### 6.4 Compaction snapshots (schema v6, redactions in v0.3.1)

`compact(up_to_seq)` takes a `VACUUM INTO` snapshot of the cell **exactly as
it stands**, registers it in `snapshots(seq, head_hash, state_digest, file,
created_at, redacted_by)` — `state_digest` is blake3 of the canonical dump of
the logical tables, the same rendering `verify` compares — and forgets the
payloads of the ops up to `up_to_seq` (≤ the head: the last ops stay
replayable). `verify` restores the snapshot into memory, checks its recorded
digest, re-applies the same cut to the restored copy (idempotent for logical
snapshots, still needed for v0.3 full copies) and replays only the tail.
`changes_since` below the snapshot answers `Compacted { snapshot_seq }`: a
replica restarts from the snapshot.

**Redacted snapshots (v0.3.1).** A snapshot is a copy made before any later
`purge`, so a purge applies to every registered snapshot too: the same state
purge, the same log scrub (payloads forgotten, supersedes censored), an FTS
`optimize` and a `VACUUM`, then a new `state_digest`. The snapshot's row
keeps its `head_hash` — the historical record of where it was taken — while
`redacted_by` records the op ids of the purges applied to the file, as a JSON
array (`[]` when none was). The censor inside a snapshot uses the same
**deterministic** salt as the live censor (blake3 over the op id), so both
scrubs land on the same payload digests and the same chain; a supersede
whose payload the live log already forgot with the compaction cut keeps its
original digest there, so inside the snapshot its body is only deleted.
`verify` accepts a redacted snapshot exactly when the tail replay — the
redacting purges included, they are ordinary ops of the tail — reproduces
the live state: the digest binds the file, the replay binds the state, and
a redaction that diverged from the live log fails the comparison. A
never-redacted snapshot must additionally match its recorded `head_hash`
byte for byte; after a redaction the in-file head hash may differ (the
censor rehashes the tail), which is why the position is found by the
snapshot's last op id, which no rehash ever touches.

## 7. Folders

**Engramma export**: `<dir>/<type>/<slug>-<id8>.md` (`id8` = the **last 8**
characters of the ULID, its random part — the first 8 are a timestamp and
collide in bulk imports; `slug` = kebab-case of the title, ASCII-folded,
truncated to 60 chars, `note` when empty) plus `engramma.json`
`{"format_version": 0, "cell": "<name>", "exported_at": "<RFC3339>", "note_count": N}`.
Tombstones (`status: deleted`) are not exported; superseded notes are.

**Obsidian import**: `.md` files under the given folder. Hidden entries (name
starting with `.`, e.g. `.obsidian/`, `.trash/`) and non-Markdown files are
skipped, symlinks are never followed and nothing outside the root is read. A
note without Engramma frontmatter is imported with a fresh `id`, `type: note`,
`source: import:obsidian/<relative path>`, `confidence: stated`,
`created_at`/`updated_at` from the file modification time, `status: active`.
An Engramma folder imported as Engramma reproduces the same state (notes and
relations) in a fresh cell.
