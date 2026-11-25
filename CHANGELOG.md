# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

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
