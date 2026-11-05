# Engramma note format — specification v0

Status: **draft, unstable** (v0.1). Fields are in English; content is in the user's
language. This document is normative for `engramma-core::format` in v0.1.

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

## 6. Change log: operation and hash rules (informative summary)

Every cell mutation is one row in `ops`. The row carries `seq` (insert order),
`op_id` (ULID), `hlc`, `actor`, `kind`, `note_id` and `payload_json`. The
canonical payload JSON has **object keys sorted lexicographically at every
level, no whitespace** (serde_json with BTreeMaps; floats are emitted in Rust's
shortest round-trip form and are therefore avoided in payloads — timestamps and
dates are strings).

The payload always carries an **envelope** with `op_id`, `hlc`, `actor`,
`kind` and `note_id` (`null` when the op has none): these fields are inside
the hashed bytes, so the hash binds the whole logical row. The columns of
`ops` are an index over the payload: reading a log, the columns must agree
with the envelope — a disagreement is a verification error even when the hash
chain itself recomputes cleanly.

Hash chain: `hash = blake3(prev_hash || payload_json_canonical)` where
`prev_hash` is the `hash` of the previous op (ASCII lowercase hex, 64 chars) or
64 `0` chars for the first op. `verify_log` recomputes the whole chain.

`apply(state, op)` is deterministic: IDs, HLC and timestamps are generated when
the operation is **created** and travel inside the payload; `apply` reads no
clock, no randomness, no I/O.

### 6.1 Golden vector of the hash rule

A fixed input with its expected hash, so that any silent change of the
canonical JSON (for instance a dependency enabling serde_json's
`preserve_order`, which would stop sorting map keys) breaks a test instead of
every cell ever written:

```text
prev_hash = 0000000000000000000000000000000000000000000000000000000000000000
payload   = {"actor":"engramma","hlc":"01a17720ca0000000007.0123456789ab",
             "kind":"init","note_id":null,"op_id":"01F7ZQ6VTSATW2BQ5Z9R0P1S8T","x":1}
hash      = a217239290ba91e17196ab1812bd01a1e3d05e71f6048387017212636c555d46
```

The reference implementation builds that object by inserting the keys in a
different order and serialises with `serde_json` (maps are BTreeMaps: keys
sorted at every level, no whitespace).

## 7. Folders

**Engramma export**: `<dir>/<type>/<slug>-<id8>.md` (`id8` = first 8 chars of the
ULID; `slug` = kebab-case of the title, ASCII-folded, truncated to 60 chars,
`note` when empty) plus `engramma.json`
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
