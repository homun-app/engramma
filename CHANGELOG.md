# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [0.6.19-beta.1] - 2026-11-26

### Changed

- The interactive confirmations (`purge`, the bulk `revert`, `oauth
  approve`, the SAS of a pairing) say `[y/N]` and read `y` and `yes`. `s`
  still confirms, for who has scripts or habits with the beta; the SAS
  prompt no longer reads `si` or `sì`.
- The human output of the CLI, the review page, the MCP server and the
  OAuth pages is in English. Who parsed the textual output must switch
  to `--json`, which does not change — except for the prose fields listed
  in the REPORT. Exit codes, JSON keys and enumerated values, OAuth
  error codes and `e.code` in the Node binding are the same.
- The prose the CLI writes INTO the registry is in English too (the
  reason of a decision, of a revert). Cells created before keep the
  Italian reasons they already hold, and `engramma decisions` shows
  them. The first device of a new data root is called `laptop`
  (`portatile` before).
- `engramma compact` takes `--experimental`; `--sperimentale`, the
  beta's name, still answers.
- The review page moved to English addresses: `/queue`, `/notes`,
  `/notes/<id>`, `/decisions`, `/policy`, `/enter`, `/style.css`. The
  beta's addresses (`/coda`, `/note`, `/note/<id>`, `/decisioni`,
  `/politica`, `/stile.css`, `/entra`) answer 308, for good: a bookmark
  lands on the new page, and the entry's token and a note's id ride
  along. The forms are renamed with their routes (`text`, `reason`,
  `version`, `since`, `until`, `confirm`); the old POST routes are gone,
  so a tab left open on the beta's page gets a 404 instead of a decision.
- The MCP server speaks English: the confirmation form (its message and
  the description of its field), the prose fields `note` and `reason` of
  the tool answers, the lines it writes on stderr, the examples in the
  tool schemas. The reason a form writes into the registry is English
  for the decisions to come (`confirmed from the client's form`).
- The OAuth pages and the token endpoint speak English, the
  `error_description` values included (the `error` codes do not change).
  The waiting page lives at `/pending/<handle>`; the beta's
  `/attesa/<handle>` answers 308, for good. `engramma oauth log` writes
  the `detail` of the events to come in English.
- The sync service speaks English: its refusals (the pairing, the share,
  the removal of a device), its warnings, the reasons an operation could
  not leave, the lines of `ENGRAMMA_TRACE`. In JSON the `why_not` of a
  device and the `relay_warning` follow.
- The reasons `vault sync`, `purge` and `cell drop` report are in English:
  an unreachable view, an orphan `.conflitto` copy, a file that would
  change a note's status, the guard against a mass deletion. The
  `.conflitto` suffix of the files stays: it is a name on disk.
- The Node binding's error messages are in English — the `Error` a
  client catches keeps its `e.code` — and so is the default name of the
  first device of `sync()` (`laptop`).
- `docs/cli.md` opens with a glossary, the English words against the
  Italian ones the other documents keep; what it quotes, its placeholders
  and its flags follow the CLI. The README's sample is the real output.

### Fixed

- The decisions page of `engramma review` no longer renders markup that
  arrives in a URL: what it shows after a POST is built by the server
  itself, every value escaped where it is built.
- `/token` answers `server_error` (RFC 6749 §5.2) when the authorization
  store cannot be read. It used to answer with the file's name,
  `oauth.db`, which is not an error code.

## [0.6.18-beta.1] - 2026-11-25

For the beta developers — the propose/review/accept/revert loop is what
this release is about. Four defects in it were found by capturing the
public documentation session, and every one is fixed here with the test
that failed on the previous release first.

### Fixed

- `revert <id> --dry-run` was IGNORED on a single decision id: the
  preview performed the revert for real and the proposal went back to
  the queue. Anyone who ran it has already cancelled something — check
  `engramma decisions` (the bulk path honoured the flag and is
  unchanged). The dry run now previews through the same classifier the
  real revert runs, writes nothing, and answers `dry_run: true` in
  JSON.
- `revert --by mcp:<actor>` was silently accepted and undid nothing:
  decisions are signed by WHO DECIDES (`user@cli`, `reviewer:<nome>`),
  never by who proposes. It is a usage error now (exit 2, with the
  fix suggested in the message). The undo of what an assistant filled
  the queue with is the new `--proposed-by <actor>` (exact or prefix),
  on `revert` and on `decisions`; it combines with `--by`, `--since`
  and `--until`, honours `--dry-run` and the `[s/N]` confirmation, and
  purged decisions keep counting among the `purged`.
- Two `supersede` proposals on the same note, accepted one after the
  other, left TWO ACTIVE notes for the same fact (and no conflict
  copy). Like a git merge, a correction is worth against the version it
  corrected: the second accept now refuses with `StaleTarget`
  (CLI exit 4; the Node binding raises `e.code === 'STALE_TARGET'`
  naming the closed target and the note that now carries the fact).
  The proposal stays in the queue; when the first correction is
  reverted, the second is decidable again. A purged target stays
  decidable as before (v0.6.11).
- Reverting a decision that ANOTHER correction stands on went through
  and left two active versions of the same fact (P corrects T, C
  corrects P, both accepted: reverting P reopened T under a living C,
  and accepting P again put P back under C). It was already in
  0.6.17-beta.1 and `verify` does not flag it. Like a commit in git,
  that revert is now a conflict naming the correction to revert first;
  a bulk revert goes newest first, so selecting both undoes both. An
  accept that would not land on the current version — over a proposal
  still in the queue, over a target another correction already closed,
  or of a proposal a standing correction already corrected — refuses
  with `StaleTarget`. A cell where 0.6.17 already left two active
  versions is not repaired automatically: reject or revert the extra
  one (`engramma decisions`, `engramma revert`).
- A proposal that was accepted had no vector until `engramma embed`
  ran: a question with no word in common did not find the note even
  though it was active (`copertura vettori: 8/14 note` in the capture).
  `proposals accept`, `proposals review` and the MCP auto reviewer now
  compute the vectors of the notes they make active, the same way
  `add` does (`--no-embed` skips it on `accept`). In MCP the vector is
  computed in the same process, after the decision, outside the write
  lock — a failure leaves the note waiting for `engramma embed` and is
  never an error of the decision. The Node binding is unchanged: Homun
  keeps its own embed cycle.

### Changed

- `engramma mcp --actor` without `--write` was an internal error from
  inside the server (`mcp: …`, exit 4): an MCP client only saw a server
  that dies. It is a CLI usage error now (exit 2, same message).
- The `--compat` without `--http` error message no longer carries
  broken spacing.

## [0.6.17-beta.1] - 2026-11-25

First public developer preview of the Engramma memory engine: the
`engramma` CLI and the MCP server, with prebuilt binaries and a shell
installer for macOS arm64 and Linux (arm64, x86_64).

- Memory cells are single SQLite files: notes (Markdown), graph, tags,
  validity, sources, confidence — every write is an operation in a
  hash-chained log, and `engramma verify` replays it end to end.
- Mixed search: text (BM25, Italian stemming included), meaning (local
  embeddings), graph and time — and every result explains itself.
- MCP for Claude and other clients, over stdio and HTTP (OAuth 2.1):
  five read-only tools, plus opt-in proposals with review and rollback.
- End-to-end encrypted sync between one owner's devices.
- The data promise, written down: future versions open your cells,
  older versions refuse them without touching a byte
  (`docs/compatibilita.md`).
