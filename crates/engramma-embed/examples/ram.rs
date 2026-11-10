// SPDX-License-Identifier: Apache-2.0

//! v0.4.4 (P1/P2): the builtin embedder's RSS in a bare Rust process, on
//! the SYSTEM allocator — the reference the mimalloc runs compare against.
//!
//! `cargo run -p engramma-embed --release --example ram`

#[path = "support/ram_run.rs"]
mod ram_run;

fn main() {
    ram_run::run();
}
