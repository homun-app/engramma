// SPDX-License-Identifier: Apache-2.0

//! v0.4.4 (P1/P2): [`ram`] under **mimalloc**, the allocator of the two
//! final artifacts (the `.node` of Homun and the CLI binary). The library
//! stays allocator-neutral; an example binary picks its own.
//!
//! `cargo run -p engramma-embed --release --example ram_mimalloc`

#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[path = "support/ram_run.rs"]
mod ram_run;

fn main() {
    ram_run::run();
}
