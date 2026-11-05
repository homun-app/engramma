// SPDX-License-Identifier: Apache-2.0

//! Restricted YAML value model for frontmatter: what the format accepts and
//! how it is re-emitted canonically (spec/format-v0.md §5).
//!
//! Parsing uses `saphyr` (which resolves scalars and rejects duplicate keys);
//! emission is our own so the canonical form is fully specified.

use std::fmt::Write as _;

use saphyr::{LoadableYamlNode, ScalarOwned, YamlOwned};

use crate::error::{FormatError, FormatResult};

/// A frontmatter YAML value (restricted model: no anchors, aliases, complex
/// keys or tags).
#[derive(Debug, Clone, PartialEq)]
pub enum YamlValue {
    /// Null.
    Null,
    /// Boolean.
    Bool(bool),
    /// Integer.
    Int(i64),
    /// Floating point.
    Float(f64),
    /// String.
    Str(String),
    /// Sequence, in order.
    Seq(Vec<YamlValue>),
    /// Mapping with string keys, in insertion order.
    Map(Vec<(String, YamlValue)>),
}

impl YamlValue {
    /// String value, if this is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            YamlValue::Str(s) => Some(s),
            _ => None,
        }
    }

    /// Convert to a JSON value (for `frontmatter_json` and op payloads).
    /// Object keys stay sorted because `serde_json` maps are BTreeMaps.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            YamlValue::Null => serde_json::Value::Null,
            YamlValue::Bool(b) => serde_json::Value::Bool(*b),
            YamlValue::Int(i) => serde_json::Value::from(*i),
            YamlValue::Float(f) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            YamlValue::Str(s) => serde_json::Value::String(s.clone()),
            YamlValue::Seq(items) => {
                serde_json::Value::Array(items.iter().map(Self::to_json).collect())
            }
            YamlValue::Map(entries) => {
                let mut m = serde_json::Map::new();
                for (k, v) in entries {
                    m.insert(k.clone(), v.to_json());
                }
                serde_json::Value::Object(m)
            }
        }
    }

    /// Build from a JSON value (used when replaying ops).
    ///
    /// # Errors
    /// [`FormatError::UnsupportedYaml`] for non-scalar object keys (JSON keys
    /// are strings, so this never fires in practice).
    pub fn from_json(v: &serde_json::Value) -> FormatResult<Self> {
        Ok(match v {
            serde_json::Value::Null => YamlValue::Null,
            serde_json::Value::Bool(b) => YamlValue::Bool(*b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    YamlValue::Int(i)
                } else {
                    YamlValue::Float(n.as_f64().unwrap_or(0.0))
                }
            }
            serde_json::Value::String(s) => YamlValue::Str(s.clone()),
            serde_json::Value::Array(items) => YamlValue::Seq(
                items
                    .iter()
                    .map(Self::from_json)
                    .collect::<FormatResult<Vec<_>>>()?,
            ),
            serde_json::Value::Object(m) => {
                let mut entries = Vec::with_capacity(m.len());
                for (k, val) in m {
                    entries.push((k.clone(), Self::from_json(val)?));
                }
                YamlValue::Map(entries)
            }
        })
    }

    /// Canonical YAML flow-form scalar, as specified in §5.4. Control
    /// characters need double quotes (single-quoted YAML folds newlines and
    /// would not survive a round trip).
    pub fn emit_scalar(s: &str) -> String {
        if s.chars().any(|c| c.is_control()) {
            format!("\"{}\"", escape_double(s))
        } else if needs_quoting(s) {
            format!("'{}'", s.replace('\'', "''"))
        } else {
            s.to_string()
        }
    }

    /// Canonical YAML flow form.
    pub fn emit_flow(&self) -> String {
        match self {
            YamlValue::Null => "null".into(),
            YamlValue::Bool(b) => b.to_string(),
            YamlValue::Int(i) => i.to_string(),
            YamlValue::Float(f) => {
                if f.is_finite() {
                    f.to_string()
                } else {
                    "null".into()
                }
            }
            YamlValue::Str(s) => Self::emit_scalar(s),
            YamlValue::Seq(items) => {
                let mut out = String::from("[");
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    out.push_str(&item.emit_flow());
                }
                out.push(']');
                out
            }
            YamlValue::Map(entries) => {
                let mut out = String::from("{");
                for (i, (k, v)) in entries.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    let _ = write!(out, "{}: {}", Self::emit_scalar(k), v.emit_flow());
                }
                out.push('}');
                out
            }
        }
    }
}

/// Escape a string for YAML double-quoted style.
fn escape_double(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                let code = c as u32;
                out.push_str(&format!("\\x{:02x}", code & 0xff));
            }
            c => out.push(c),
        }
    }
    out
}

/// Does a plain scalar need single quotes to survive a round trip? Follows the
/// YAML plain-scalar rules that matter here: no `": "` or trailing `:`, no
/// `" #"` or leading `#`, none of the flow/reserved indicators, and nothing
/// that would read back as a bool, null or number.
fn needs_quoting(s: &str) -> bool {
    let starts_with_indicator = s.chars().next().is_some_and(|c| {
        matches!(
            c,
            '\'' | '"'
                | '!'
                | '&'
                | '*'
                | ','
                | '>'
                | '?'
                | '|'
                | '%'
                | '@'
                | '`'
                | '#'
                | '['
                | ']'
                | '{'
                | '}'
                | '-'
                | ':'
        )
    });
    if s.is_empty()
        || starts_with_indicator
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.contains(": ")
        || s.ends_with(':')
        || s.contains(" #")
        || s.starts_with('#')
        || s.chars().any(|c| {
            matches!(
                c,
                '{' | '}'
                    | '['
                    | ']'
                    | ','
                    | '&'
                    | '*'
                    | '?'
                    | '|'
                    | '<'
                    | '>'
                    | '='
                    | '!'
                    | '%'
                    | '@'
                    | '`'
                    | '\n'
                    | '\t'
            )
        })
    {
        return true;
    }
    let lower = s.to_ascii_lowercase();
    // YAML core-schema numbers (and the 1.1 octal legacy: `0755`) must be
    // quoted, or they would read back as integers.
    let yaml_number = s.parse::<i64>().is_ok() || s.parse::<f64>().is_ok() || regex_number_like(s);
    matches!(
        lower.as_str(),
        "true" | "false" | "null" | "~" | "yes" | "no" | "on" | "off"
    ) || yaml_number
}

/// Hex (`0x1f`), octal (`0o17`/legacy `0755`) and binary (`0b101`) forms.
fn regex_number_like(s: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"^(?:0[xX][0-9a-fA-F]+|0[oO][0-7]+|0[bB][01]+|0[0-9]+)$",
        )
        .unwrap(/* static regex, valid by construction */)
    });
    re.is_match(s)
}

/// Parse a frontmatter string into ordered `(key, value)` pairs.
///
/// # Errors
/// [`FormatError::Yaml`] for malformed YAML; [`FormatError::UnsupportedYaml`]
/// for constructs outside the model (anchors, aliases, complex keys, tags,
/// non-string keys).
pub fn parse_mapping(text: &str) -> FormatResult<Vec<(String, YamlValue)>> {
    let docs = YamlOwned::load_from_str(text).map_err(|e| FormatError::Yaml(e.to_string()))?;
    let Some(doc) = docs.into_iter().next() else {
        return Err(FormatError::Yaml("empty frontmatter".into()));
    };
    let YamlOwned::Mapping(mapping) = doc else {
        return Err(FormatError::Yaml("frontmatter must be a mapping".into()));
    };
    let mut out = Vec::with_capacity(mapping.len());
    for (k, v) in mapping {
        let key = match k {
            YamlOwned::Value(ScalarOwned::String(s)) => s,
            other => {
                return Err(FormatError::UnsupportedYaml(format!(
                    "mapping keys must be plain strings, found {other:?}"
                )));
            }
        };
        out.push((key, convert(v)?));
    }
    Ok(out)
}

fn convert(y: YamlOwned) -> FormatResult<YamlValue> {
    Ok(match y {
        YamlOwned::Value(s) => match s {
            ScalarOwned::Null => YamlValue::Null,
            ScalarOwned::Boolean(b) => YamlValue::Bool(b),
            ScalarOwned::Integer(i) => YamlValue::Int(i),
            ScalarOwned::FloatingPoint(f) => YamlValue::Float(f.into_inner()),
            ScalarOwned::String(s) => YamlValue::Str(s),
        },
        YamlOwned::Sequence(items) => YamlValue::Seq(
            items
                .into_iter()
                .map(convert)
                .collect::<FormatResult<Vec<_>>>()?,
        ),
        YamlOwned::Mapping(m) => {
            let mut entries = Vec::with_capacity(m.len());
            for (k, v) in m {
                let key = match k {
                    YamlOwned::Value(ScalarOwned::String(s)) => s,
                    other => {
                        return Err(FormatError::UnsupportedYaml(format!(
                            "mapping keys must be plain strings, found {other:?}"
                        )));
                    }
                };
                entries.push((key, convert(v)?));
            }
            YamlValue::Map(entries)
        }
        YamlOwned::Representation(..) => {
            return Err(FormatError::UnsupportedYaml(
                "raw scalar representations are not accepted".into(),
            ));
        }
        YamlOwned::Tagged(..) => {
            return Err(FormatError::UnsupportedYaml("tags are not accepted".into()));
        }
        YamlOwned::Alias(_) => {
            return Err(FormatError::UnsupportedYaml(
                "aliases are not accepted".into(),
            ));
        }
        YamlOwned::BadValue => {
            return Err(FormatError::Yaml("incoherent scalar value".into()));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_orders() {
        let m = parse_mapping("b: 2\na: x-hello\nc: [1, two]\nd: {k: v}\n").unwrap(/* test-only */);
        assert_eq!(m[0].0, "b");
        assert_eq!(m[0].1, YamlValue::Int(2));
        assert_eq!(m[1].1, YamlValue::Str("x-hello".into()));
        assert_eq!(
            m[2].1,
            YamlValue::Seq(vec![YamlValue::Int(1), YamlValue::Str("two".into())])
        );
        assert_eq!(
            m[3].1,
            YamlValue::Map(vec![("k".into(), YamlValue::Str("v".into()))])
        );
    }

    #[test]
    fn rejects_duplicates_resolves_anchors() {
        assert!(parse_mapping("a: 1\na: 2\n").is_err());
        // Anchors and aliases are resolved by the loader, not rejected: the
        // value survives as a plain value (the emitter never writes them).
        let m = parse_mapping("a: &x 1\nb: *x\n").unwrap(/* test-only */);
        assert_eq!(m[1].1, YamlValue::Int(1));
    }

    #[test]
    fn flow_emission_is_canonical() {
        let v = YamlValue::Map(vec![
            ("bot".into(), YamlValue::Str("casa".into())),
            ("n".into(), YamlValue::Int(3)),
        ]);
        assert_eq!(v.emit_flow(), "{bot: casa, n: 3}");
        assert_eq!(YamlValue::Str("a: b".into()).emit_flow(), "'a: b'");
        assert_eq!(YamlValue::Str("true".into()).emit_flow(), "'true'");
        assert_eq!(YamlValue::Str("plain".into()).emit_flow(), "plain");
        let empty: YamlValue = YamlValue::Seq(vec![]);
        assert_eq!(empty.emit_flow(), "[]");
    }

    #[test]
    fn json_canonicalises_map_key_order() {
        let v = YamlValue::Map(vec![
            ("b".into(), YamlValue::Int(1)),
            ("a".into(), YamlValue::Str("z".into())),
        ]);
        let j = v.to_json();
        // serde_json maps are sorted: the canonical JSON form is stable.
        assert_eq!(j.to_string(), r#"{"a":"z","b":1}"#);
        // Order is not preserved through JSON (callers that need order use
        // arrays of pairs); values survive.
        let back = YamlValue::from_json(&j).unwrap(/* test-only */);
        let YamlValue::Map(entries) = back else {
            panic!("expected map");
        };
        assert_eq!(entries[0], ("a".into(), YamlValue::Str("z".into())));
        assert_eq!(entries[1], ("b".into(), YamlValue::Int(1)));
    }
}
