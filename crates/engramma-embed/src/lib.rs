// SPDX-License-Identifier: Apache-2.0
//! Engramma embedders (v0.2): built-in ONNX model via tract, Ollama, and
//! external OpenAI-compatible HTTP endpoints. See `docs/DESIGN.md` § 7.

pub mod builtin;
pub mod config;
pub mod http;
pub mod ollama;
pub mod pull;
pub mod spec;
#[cfg(test)]
mod testserver;
pub mod verify;

pub use builtin::Builtin;
pub use http::Http;
pub use ollama::Ollama;
pub use spec::{ModelSpec, Pooling, model_root, resolve};
