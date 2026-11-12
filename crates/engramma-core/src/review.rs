// SPDX-License-Identifier: Apache-2.0

//! v0.5 (P2): the automatic reviewers. Two kinds, both configured in
//! `reviewers.yaml` at the root of the data:
//!
//! - `command`: an external program with FIXED arguments (no shell). It
//!   receives the request JSON on stdin and answers JSON on stdout, within
//!   a time limit. Good for any model, any rule, and for the tests.
//! - `openai`: an OpenAI-compatible `/v1/chat/completions` endpoint with
//!   JSON-schema output (`base_url`, `model`, `api_key_env`, time limit).
//!   Covers OpenAI itself, local servers (llama-server, Ollama) and every
//!   provider with a compatible interface.
//!
//! The file holds the NAME of the environment variable carrying the key,
//! never the key. Every failure — a timeout, a crash, an unreadable answer,
//! a missing reviewer — fails safe: the proposal stays with the human.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::error::CellError;
use crate::format::yaml::{self, YamlValue};

/// The prompt, versioned in the repo: its blake3 enters every automatic
/// decision, so the history can say under WHICH prompt a model decided.
const PROMPT_V1: &str = include_str!("review/prompt-v1.md");

/// The prompt's version (bump on any change of `prompt-v1.md`).
pub const PROMPT_VERSION: u32 = 1;

/// blake3 of the prompt file — stable within the process.
#[must_use]
pub fn prompt_hash() -> String {
    blake3::hash(PROMPT_V1.as_bytes()).to_hex().to_string()
}

/// The prompt text (the tests pin its contract: data, not instructions).
#[must_use]
pub fn prompt_text() -> &'static str {
    PROMPT_V1
}

// ---------------------------------------------------------------------------
// the request
// ---------------------------------------------------------------------------

/// What a reviewer sees. Everything personal rides between explicit
/// delimiters, and the prompt says those blocks are data.
#[derive(Debug, Clone)]
pub struct ReviewRequest {
    /// The cell's name.
    pub cell: String,
    /// The proposal: text, title, kind, tags, confidence, mode, reason.
    pub proposal: Value,
    /// The text of the supersede target, when a correction.
    pub target_text: Option<String>,
    /// Up to 5 similar ACTIVE notes: `(id, text)`.
    pub similar: Vec<(String, String)>,
    /// The policy summary (the fence).
    pub policy: Value,
}

impl ReviewRequest {
    /// The JSON sent to the reviewer.
    #[must_use]
    pub fn to_json(&self) -> Value {
        // every personal block rides between explicit, named delimiters —
        // the prompt says those blocks are data, never instructions
        let delimited =
            |what: &str, text: &str| format!("<<<BEGIN {what}>>>\n{text}\n<<<END {what}>>>");
        json!({
            "prompt_version": PROMPT_VERSION,
            "cell": self.cell,
            "policy": self.policy,
            "proposal": {
                "text": delimited("PROPOSAL TEXT", self.proposal["text"].as_str().unwrap_or_default()),
                "title": self.proposal["title"],
                "kind": self.proposal["kind"],
                "tags": self.proposal["tags"],
                "confidence": self.proposal["confidence"],
                "mode": self.proposal["mode"],
                "reason": self.proposal["reason"],
            },
            "target_text": self.target_text.as_deref().map(|t| delimited("TARGET TEXT", t)),
            "similar": self
                .similar
                .iter()
                .map(|(id, text)| {
                    json!({ "id": id, "text": delimited("EXISTING NOTE", text) })
                })
                .collect::<Vec<_>>(),
        })
    }
}

/// What the reviewer answered. A transport-level failure (spawn, timeout,
/// crash, unreadable body) is [`ReviewVerdict::Failed`]; a well-formed
/// answer with a strange decision, confidence or reason is
/// [`ReviewVerdict::Escalate`] — the severe reading: anything but a clean
/// decision is a doubt, and a doubt goes to the human.
#[derive(Debug, Clone)]
pub enum ReviewVerdict {
    /// Store it.
    Accept {
        /// Why, as the reviewer said it.
        reason: String,
        /// The declared confidence, 0..=1.
        confidence: f64,
    },
    /// Refuse it.
    Reject {
        /// Why.
        reason: String,
        /// The declared confidence, 0..=1.
        confidence: f64,
    },
    /// Pass to the human — the reviewer's own choice.
    Escalate {
        /// Why.
        reason: String,
        /// The declared confidence, 0..=1.
        confidence: f64,
    },
    /// The reviewer could not answer (error, timeout, unreadable): the
    /// proposal stays pending, marked «revisione automatica fallita».
    Failed {
        /// What happened, for the log surface (never the content).
        error: String,
    },
}

/// The strict reading of one answer body: `{"decision","reason","confidence"}`.
fn read_verdict(body: &str) -> ReviewVerdict {
    let parsed: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => {
            return ReviewVerdict::Failed {
                error: "unreadable answer".into(),
            };
        }
    };
    let reason = parsed
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .chars()
        .take(500)
        .collect::<String>();
    let Some(confidence) = parsed.get("confidence").and_then(Value::as_f64) else {
        return ReviewVerdict::Escalate {
            reason: "the reviewer did not declare a confidence".into(),
            confidence: 0.0,
        };
    };
    if !(0.0..=1.0).contains(&confidence) {
        return ReviewVerdict::Escalate {
            reason: "the reviewer declared a confidence outside [0, 1]".into(),
            confidence: 0.0,
        };
    }
    match parsed.get("decision").and_then(Value::as_str) {
        Some("accept") => ReviewVerdict::Accept { reason, confidence },
        Some("reject") => ReviewVerdict::Reject { reason, confidence },
        // the severe reading: ANYTHING else is a doubt
        _ => ReviewVerdict::Escalate {
            reason: if reason.is_empty() {
                "the reviewer answered something else".into()
            } else {
                reason
            },
            confidence,
        },
    }
}

// ---------------------------------------------------------------------------
// the configuration (reviewers.yaml)
// ---------------------------------------------------------------------------

/// One configured reviewer.
#[derive(Debug, Clone)]
pub struct ReviewerDef {
    /// The reviewer's name (the policy's `reviewer`).
    pub name: String,
    /// How to talk to it.
    pub kind: ReviewerKind,
}

/// How to talk to a reviewer.
#[derive(Debug, Clone)]
pub enum ReviewerKind {
    /// An external program with fixed arguments: no shell, ever.
    Command {
        /// The program and its fixed arguments.
        argv: Vec<String>,
        /// Time limit (default 30 s).
        timeout: Duration,
    },
    /// An OpenAI-compatible endpoint with JSON-schema output.
    Openai {
        /// The base URL (`/v1/chat/completions` is appended when the base
        /// does not already end with it).
        base_url: String,
        /// The model name.
        model: String,
        /// The ENVIRONMENT VARIABLE holding the key (never the key).
        api_key_env: String,
        /// Time limit (default 30 s).
        timeout: Duration,
    },
}

/// The parsed `reviewers.yaml`.
#[derive(Debug, Clone, Default)]
pub struct Reviewers {
    defs: HashMap<String, ReviewerDef>,
}

fn yaml_err(what: &str) -> CellError {
    CellError::Invalid(format!("reviewers.yaml: {what}"))
}

fn field<'a>(map: &'a [(String, YamlValue)], key: &str) -> Option<&'a YamlValue> {
    map.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

impl Reviewers {
    /// Load `reviewers.yaml` from a data root. A missing file is no error:
    /// there simply are no reviewers (and `write: auto` will fail safe).
    ///
    /// # Errors
    /// [`CellError::Invalid`] when the file exists but is malformed.
    pub fn load(root: &Path) -> Result<Self, CellError> {
        let path = root.join("reviewers.yaml");
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(Self::default());
        };
        Self::from_yaml_str(&text)
    }

    /// Parse the file's content. Strict: unknown fields and — above all —
    /// any raw `api_key` are refused.
    ///
    /// # Errors
    /// [`CellError::Invalid`] on malformed content.
    pub fn from_yaml_str(text: &str) -> Result<Self, CellError> {
        let top = yaml::parse_mapping(text).map_err(|e| yaml_err(&e.to_string()))?;
        let Some(YamlValue::Map(reviewers)) = field(&top, "reviewers") else {
            return Err(yaml_err("must hold a `reviewers:` mapping"));
        };
        let mut defs = HashMap::new();
        for (name, def) in reviewers {
            let YamlValue::Map(fields) = def else {
                return Err(yaml_err(&format!("`{name}` must be a mapping")));
            };
            let def = if let Some(cmd) = field(fields, "command") {
                let YamlValue::Map(cfields) = cmd else {
                    return Err(yaml_err(&format!("`{name}.command` must be a mapping")));
                };
                if field(fields, "openai").is_some() {
                    return Err(yaml_err(&format!(
                        "`{name}`: command and openai are exclusive"
                    )));
                }
                let unknown: Vec<&String> = cfields
                    .iter()
                    .map(|(k, _)| k)
                    .filter(|k| k.as_str() != "argv" && k.as_str() != "timeout_secs")
                    .collect();
                if let Some(k) = unknown.first() {
                    return Err(yaml_err(&format!(
                        "`{name}.command`: unknown field `{k}` (argv, timeout_secs)"
                    )));
                }
                let Some(YamlValue::Seq(argv)) = field(cfields, "argv") else {
                    return Err(yaml_err(&format!(
                        "`{name}.command.argv` must be a list (no shell, fixed arguments)"
                    )));
                };
                let argv: Vec<String> = argv
                    .iter()
                    .map(|v| {
                        v.as_str().map(String::from).ok_or_else(|| {
                            yaml_err(&format!("`{name}.command.argv` must hold strings"))
                        })
                    })
                    .collect::<Result<_, _>>()?;
                if argv.is_empty() {
                    return Err(yaml_err(&format!("`{name}.command.argv` cannot be empty")));
                }
                let timeout = timeout_of(cfields, &format!("{name}.command"))?;
                ReviewerDef {
                    name: name.clone(),
                    kind: ReviewerKind::Command { argv, timeout },
                }
            } else if let Some(oai) = field(fields, "openai") {
                let YamlValue::Map(ofields) = oai else {
                    return Err(yaml_err(&format!("`{name}.openai` must be a mapping")));
                };
                for forbidden in ["api_key", "key", "token"] {
                    if field(ofields, forbidden).is_some() {
                        return Err(yaml_err(&format!(
                            "`{name}.openai`: `{forbidden}` must never appear — the file \
                             carries the NAME of the environment variable (`api_key_env`)"
                        )));
                    }
                }
                let unknown: Vec<&String> = ofields
                    .iter()
                    .map(|(k, _)| k)
                    .filter(|k| {
                        !matches!(
                            k.as_str(),
                            "base_url" | "model" | "api_key_env" | "timeout_secs"
                        )
                    })
                    .collect();
                if let Some(k) = unknown.first() {
                    return Err(yaml_err(&format!(
                        "`{name}.openai`: unknown field `{k}` (base_url, model, api_key_env, timeout_secs)"
                    )));
                }
                let get_s = |key: &str| -> Result<String, CellError> {
                    field(ofields, key)
                        .and_then(YamlValue::as_str)
                        .map(String::from)
                        .ok_or_else(|| yaml_err(&format!("`{name}.openai.{key}` is required")))
                };
                let base_url = get_s("base_url")?;
                let model = get_s("model")?;
                let api_key_env = get_s("api_key_env")?;
                let timeout = timeout_of(ofields, &format!("{name}.openai"))?;
                ReviewerDef {
                    name: name.clone(),
                    kind: ReviewerKind::Openai {
                        base_url,
                        model,
                        api_key_env,
                        timeout,
                    },
                }
            } else {
                return Err(yaml_err(&format!(
                    "`{name}` must hold a `command:` or an `openai:` mapping"
                )));
            };
            defs.insert(name.clone(), def);
        }
        Ok(Self { defs })
    }

    /// A reviewer by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ReviewerDef> {
        self.defs.get(name)
    }
}

fn timeout_of(fields: &[(String, YamlValue)], where_: &str) -> Result<Duration, CellError> {
    match field(fields, "timeout_secs") {
        None => Ok(Duration::from_secs(30)),
        Some(YamlValue::Int(s)) if *s > 0 => {
            Ok(Duration::from_secs(u64::try_from(*s).unwrap_or(30)))
        }
        Some(_) => Err(yaml_err(&format!(
            "`{where_}.timeout_secs` must be a positive whole number"
        ))),
    }
}

// ---------------------------------------------------------------------------
// the calls
// ---------------------------------------------------------------------------

impl ReviewerDef {
    /// The model name to record with the decision, when the reviewer names
    /// one (the `openai` kind does; a `command` is whatever it is).
    #[must_use]
    pub fn model_name(&self) -> Option<String> {
        match &self.kind {
            ReviewerKind::Command { .. } => None,
            ReviewerKind::Openai { model, .. } => Some(model.clone()),
        }
    }

    /// Ask this reviewer. Never panics, never hangs past its time limit;
    /// every transport problem is [`ReviewVerdict::Failed`].
    #[must_use]
    pub fn review(&self, request: &ReviewRequest) -> ReviewVerdict {
        match &self.kind {
            ReviewerKind::Command { argv, timeout } => run_command(argv, timeout, request),
            ReviewerKind::Openai {
                base_url,
                model,
                api_key_env,
                timeout,
            } => run_openai(base_url, model, api_key_env, timeout, request),
        }
    }
}

/// The `command` reviewer: fixed argv, request on stdin, answer on stdout,
/// a wall-clock time limit. The proposal text never reaches a shell —
/// there is none.
fn run_command(argv: &[String], timeout: &Duration, request: &ReviewRequest) -> ReviewVerdict {
    if argv.is_empty() {
        return ReviewVerdict::Failed {
            error: "empty argv".into(),
        };
    }
    let mut child = match std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            return ReviewVerdict::Failed {
                error: format!("spawn: {e}"),
            };
        }
    };
    // feed stdin from another thread: a reviewer that never reads must not
    // deadlock the write
    let payload = request.to_json().to_string();
    let stdin = child.stdin.take();
    let feeder = std::thread::spawn(move || {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(payload.as_bytes());
        }
    });
    let deadline = Instant::now() + *timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                feeder.join().ok();
                return ReviewVerdict::Failed {
                    error: format!("wait: {e}"),
                };
            }
        }
    };
    feeder.join().ok();
    let Some(status) = status else {
        return ReviewVerdict::Failed {
            error: format!("timeout after {} s", timeout.as_secs()),
        };
    };
    if !status.success() {
        return ReviewVerdict::Failed {
            error: format!("exited with {status}"),
        };
    }
    let Some(mut out) = child.stdout.take() else {
        return ReviewVerdict::Failed {
            error: "no stdout".into(),
        };
    };
    let mut body = String::new();
    if std::io::Read::read_to_string(&mut out, &mut body).is_err() {
        return ReviewVerdict::Failed {
            error: "stdout unreadable".into(),
        };
    }
    read_verdict(&body)
}

/// The strict-output JSON schema sent along with every `openai` call: the
/// server may only answer the contract.
fn answer_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["decision", "reason", "confidence"],
        "properties": {
            "decision": { "type": "string", "enum": ["accept", "reject", "escalate"] },
            "reason": { "type": "string", "maxLength": 500 },
            "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
        }
    })
}

/// The `openai` reviewer: `/v1/chat/completions` with JSON-schema output.
fn run_openai(
    base_url: &str,
    model: &str,
    api_key_env: &str,
    timeout: &Duration,
    request: &ReviewRequest,
) -> ReviewVerdict {
    let url = if base_url.ends_with("/chat/completions") {
        base_url.to_string()
    } else {
        format!("{}/v1/chat/completions", base_url.trim_end_matches('/'))
    };
    let key = std::env::var(api_key_env).unwrap_or_default();
    let body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": format!("{PROMPT_V1}\n\n(prompt v{PROMPT_VERSION}, hash {})",
                prompt_hash()) },
            { "role": "user", "content": request.to_json().to_string() }
        ],
        "temperature": 0,
        "response_format": {
            "type": "json_schema",
            // `strict` belongs here, beside the schema — not inside it
            "json_schema": { "name": "engramma_review", "strict": true, "schema": answer_schema() }
        }
    });
    let mut call = ureq::post(&url)
        .config()
        .timeout_global(Some(*timeout))
        .build();
    if !key.is_empty() {
        call = call.header("Authorization", format!("Bearer {key}"));
    }
    let answer = call.send_json(&body);
    let text = match answer {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let mut r = resp;
            match r.body_mut().read_to_string() {
                Ok(t) if (200..300).contains(&status) => t,
                Ok(t) => {
                    let snippet: String = t.chars().take(200).collect();
                    return ReviewVerdict::Failed {
                        error: format!("http {status}: {snippet}"),
                    };
                }
                Err(e) => {
                    return ReviewVerdict::Failed {
                        error: format!("body: {e}"),
                    };
                }
            }
        }
        Err(e) => {
            return ReviewVerdict::Failed {
                error: format!("transport: {e}"),
            };
        }
    };
    // OpenAI shape: choices[0].message.content — the content itself is the
    // contract JSON
    let content = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
        v.get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
            .and_then(|c| c.get("message"))
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .map(String::from)
    });
    match content {
        Some(content) => read_verdict(&content),
        None => ReviewVerdict::Failed {
            error: "no answer content".into(),
        },
    }
}
