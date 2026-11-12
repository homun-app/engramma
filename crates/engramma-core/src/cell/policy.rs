// SPDX-License-Identifier: Apache-2.0

//! v0.5 (P2): the per-cell policy. It is an OPERATION of the log
//! (`set_policy`), so the history says who allowed what, and when; it is
//! changed only from the CLI — never from the web, never from MCP. The
//! policy is the fence the automatic reviewer runs inside: whatever is not
//! explicitly allowed lands on the human.

use serde_json::Value;

use crate::error::CellError;
use crate::format::NoteType;

use super::Cell;

/// Who may write, and how far the machine may go on its own.
#[derive(Debug, Clone, PartialEq)]
pub enum WritePolicy {
    /// The cell accepts no proposals (the default).
    Off,
    /// Every proposal lands in the queue.
    Propose,
    /// Ask the user through an elicitation form when the client supports it
    /// (P3); otherwise the queue.
    Confirm,
    /// The configured reviewer decides, inside [`Policy`]'s limits.
    Auto,
}

impl WritePolicy {
    /// Wire name.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            WritePolicy::Off => "off",
            WritePolicy::Propose => "propose",
            WritePolicy::Confirm => "confirm",
            WritePolicy::Auto => "auto",
        }
    }

    fn parse(s: &str) -> Result<Self, CellError> {
        Ok(match s {
            "off" => WritePolicy::Off,
            "propose" => WritePolicy::Propose,
            "confirm" => WritePolicy::Confirm,
            "auto" => WritePolicy::Auto,
            other => {
                return Err(CellError::Invalid(format!(
                    "policy `write`: unknown value `{other}` (off | propose | confirm | auto)"
                )));
            }
        })
    }
}

/// Which notes an automatic supersede may close.
#[derive(Debug, Clone, PartialEq)]
pub enum AllowSupersede {
    /// None (the default): the human decides every correction.
    None,
    /// Only notes that are not `stated` by a human.
    Inferred,
    /// Any.
    Any,
}

impl AllowSupersede {
    /// Wire name.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            AllowSupersede::None => "none",
            AllowSupersede::Inferred => "inferred",
            AllowSupersede::Any => "any",
        }
    }

    fn parse(s: &str) -> Result<Self, CellError> {
        Ok(match s {
            "none" => AllowSupersede::None,
            "inferred" => AllowSupersede::Inferred,
            "any" => AllowSupersede::Any,
            other => {
                return Err(CellError::Invalid(format!(
                    "policy `allow_supersede`: unknown value `{other}` (none | inferred | any)"
                )));
            }
        })
    }
}

/// The cell's policy. Defaults when no `set_policy` op ever ran:
/// `write: off` — a cell nobody opened to proposals stays closed.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// Who may propose, and how.
    pub write: WritePolicy,
    /// The configured reviewer's name, when `write: auto`.
    pub reviewer: Option<String>,
    /// The reviewer's declared confidence under which the proposal goes to
    /// the human (default 0.8).
    pub min_confidence: f64,
    /// Note kinds the automatic path may accept (empty: all).
    pub kinds: Vec<NoteType>,
    /// Which notes an automatic supersede may close (default `none`).
    pub allow_supersede: AllowSupersede,
    /// THE BREAKER (default 20): past this many automatic decisions in an
    /// hour, every proposal goes to the human.
    pub max_auto_per_hour: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            write: WritePolicy::Off,
            reviewer: None,
            min_confidence: 0.8,
            kinds: Vec::new(),
            allow_supersede: AllowSupersede::None,
            max_auto_per_hour: 20,
        }
    }
}

impl Policy {
    /// Parse a policy document (the `set_policy` body). Unknown fields are
    /// refused: a typo'd `min_confidnce` must not silently read as a
    /// permission.
    ///
    /// # Errors
    /// [`CellError::Invalid`] on unknown or malformed fields.
    pub fn parse(v: &Value) -> Result<Self, CellError> {
        let mut p = Policy::default();
        let Some(fields) = v.as_object() else {
            return Err(CellError::Invalid("the policy must be an object".into()));
        };
        for (k, val) in fields {
            match k.as_str() {
                "write" => {
                    p.write =
                        WritePolicy::parse(val.as_str().ok_or_else(|| bad("write", "a string"))?)?;
                }
                "reviewer" => {
                    p.reviewer = Some(
                        val.as_str()
                            .ok_or_else(|| bad("reviewer", "a string"))?
                            .to_string(),
                    );
                }
                "min_confidence" => {
                    let c = val
                        .as_f64()
                        .ok_or_else(|| bad("min_confidence", "a number"))?;
                    if !(0.0..=1.0).contains(&c) {
                        return Err(CellError::Invalid(
                            "policy `min_confidence` must be within [0, 1]".into(),
                        ));
                    }
                    p.min_confidence = c;
                }
                "kinds" => {
                    let arr = val
                        .as_array()
                        .ok_or_else(|| bad("kinds", "a list of note types"))?;
                    let mut kinds = Vec::new();
                    for k in arr {
                        let s = k.as_str().ok_or_else(|| bad("kinds", "strings"))?;
                        kinds.push(
                            NoteType::parse(s)
                                .map_err(|e| CellError::Invalid(format!("policy `kinds`: {e}")))?,
                        );
                    }
                    p.kinds = kinds;
                }
                "allow_supersede" => {
                    p.allow_supersede = AllowSupersede::parse(
                        val.as_str()
                            .ok_or_else(|| bad("allow_supersede", "a string"))?,
                    )?;
                }
                "max_auto_per_hour" => {
                    p.max_auto_per_hour = u32::try_from(
                        val.as_u64()
                            .ok_or_else(|| bad("max_auto_per_hour", "a whole number"))?,
                    )
                    .map_err(|_| {
                        CellError::Invalid("policy `max_auto_per_hour` is too large".into())
                    })?;
                }
                other => {
                    return Err(CellError::Invalid(format!(
                        "unknown policy field `{other}`"
                    )));
                }
            }
        }
        if p.write == WritePolicy::Auto && p.reviewer.is_none() {
            return Err(CellError::Invalid(
                "policy `write: auto` needs a `reviewer`".into(),
            ));
        }
        Ok(p)
    }

    /// The summary the reviewer's prompt carries: the fence, in one glance.
    #[must_use]
    pub fn summary(&self) -> Value {
        serde_json::json!({
            "write": self.write.as_str(),
            "min_confidence": self.min_confidence,
            "kinds": self.kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
            "allow_supersede": self.allow_supersede.as_str(),
            "max_auto_per_hour": self.max_auto_per_hour,
        })
    }
}

fn bad(field: &str, want: &str) -> CellError {
    CellError::Invalid(format!("policy `{field}` wants {want}"))
}

impl Cell {
    /// The cell's policy: the last `set_policy` op, or the defaults
    /// (`write: off`). A malformed stored document is an error, never a
    /// silent default-open.
    ///
    /// # Errors
    /// [`CellError::Invalid`] when the stored document cannot be parsed.
    pub fn policy_of(&self) -> Result<Policy, CellError> {
        match self.policy()? {
            None => Ok(Policy::default()),
            Some(v) => Policy::parse(&v),
        }
    }
}
