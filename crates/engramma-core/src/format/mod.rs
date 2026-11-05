// SPDX-License-Identifier: Apache-2.0

//! The note format (spec/format-v0.md): model, parser, canonical serialiser.

pub mod body;
pub mod yaml;

use std::fmt::Write as _;

use time::format_description::well_known::Rfc3339;
use time::macros::format_description;
use time::{Date, OffsetDateTime, UtcOffset};
use ulid::Ulid;

use crate::error::{FormatError, FormatResult};
use body::BodyAnalysis;
use yaml::YamlValue;

/// `YYYY-MM-DD`.
const DATE_FORMAT: &[time::format_description::BorrowedFormatItem<'static>] =
    format_description!("[year]-[month]-[day]");

/// `YYYY-MM-DDTHH:MM:SSZ` (always UTC, second precision).
const DATETIME_FORMAT: &[time::format_description::BorrowedFormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

/// How unknown keys are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseMode {
    /// Unknown non-`x-` keys are errors (Engramma folders).
    Strict,
    /// Unknown non-`x-` keys produce warnings and are dropped (imports).
    Import,
}

/// Note type (`type` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteType {
    /// A fact.
    Fact,
    /// A person entity note.
    Person,
    /// A place entity note.
    Place,
    /// A project entity note.
    Project,
    /// An event entity note.
    Event,
    /// A preference (something that changes often).
    Preference,
    /// Generic note.
    Note,
}

impl NoteType {
    /// All values, in spec order.
    pub const ALL: [NoteType; 7] = [
        NoteType::Fact,
        NoteType::Person,
        NoteType::Place,
        NoteType::Project,
        NoteType::Event,
        NoteType::Preference,
        NoteType::Note,
    ];

    /// Wire name.
    pub fn as_str(&self) -> &'static str {
        match self {
            NoteType::Fact => "fact",
            NoteType::Person => "person",
            NoteType::Place => "place",
            NoteType::Project => "project",
            NoteType::Event => "event",
            NoteType::Preference => "preference",
            NoteType::Note => "note",
        }
    }

    /// Does a note of this type have a *subject entity* (its title is a name)?
    /// See spec §3: relations declared by such notes use the title entity as
    /// their subject.
    pub fn has_subject_entity(&self) -> bool {
        matches!(
            self,
            NoteType::Person
                | NoteType::Place
                | NoteType::Project
                | NoteType::Event
                | NoteType::Preference
        )
    }

    /// Parse the wire name.
    ///
    /// # Errors
    /// [`FormatError::Field`] for unknown types.
    pub fn parse(s: &str) -> FormatResult<Self> {
        Ok(match s {
            "fact" => NoteType::Fact,
            "person" => NoteType::Person,
            "place" => NoteType::Place,
            "project" => NoteType::Project,
            "event" => NoteType::Event,
            "preference" => NoteType::Preference,
            "note" => NoteType::Note,
            other => {
                return Err(FormatError::Field {
                    field: "type",
                    message: format!(
                        "`{other}` is not one of {}",
                        NoteType::ALL
                            .iter()
                            .map(Self::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            }
        })
    }
}

/// Reliability (`confidence` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// Said by the user.
    Stated,
    /// Deduced by an assistant.
    Inferred,
    /// Third-party content: born quarantined.
    External,
}

impl Confidence {
    /// Wire name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Confidence::Stated => "stated",
            Confidence::Inferred => "inferred",
            Confidence::External => "external",
        }
    }

    /// Parse the wire name.
    ///
    /// # Errors
    /// [`FormatError::Field`] for unknown values.
    pub fn parse(s: &str) -> FormatResult<Self> {
        Ok(match s {
            "stated" => Confidence::Stated,
            "inferred" => Confidence::Inferred,
            "external" => Confidence::External,
            other => {
                return Err(FormatError::Field {
                    field: "confidence",
                    message: format!("`{other}` is not one of stated, inferred, external"),
                });
            }
        })
    }
}

/// Lifecycle status (`status` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Current truth.
    Active,
    /// Unconfirmed external content: hidden from default search.
    Quarantined,
    /// Replaced by a newer note.
    Superseded,
    /// Tombstone (logical delete).
    Deleted,
}

impl Status {
    /// Wire name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Quarantined => "quarantined",
            Status::Superseded => "superseded",
            Status::Deleted => "deleted",
        }
    }

    /// Parse the wire name.
    ///
    /// # Errors
    /// [`FormatError::Field`] for unknown values.
    pub fn parse(s: &str) -> FormatResult<Self> {
        Ok(match s {
            "active" => Status::Active,
            "quarantined" => Status::Quarantined,
            "superseded" => Status::Superseded,
            "deleted" => Status::Deleted,
            other => {
                return Err(FormatError::Field {
                    field: "status",
                    message: format!(
                        "`{other}` is not one of active, quarantined, superseded, deleted"
                    ),
                });
            }
        })
    }
}

/// Supported stemming languages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// Italian.
    It,
    /// English.
    En,
    /// French.
    Fr,
    /// German.
    De,
    /// Spanish.
    Es,
    /// Portuguese.
    Pt,
}

impl Lang {
    /// Wire name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Lang::It => "it",
            Lang::En => "en",
            Lang::Fr => "fr",
            Lang::De => "de",
            Lang::Es => "es",
            Lang::Pt => "pt",
        }
    }

    /// Parse the wire name.
    ///
    /// # Errors
    /// [`FormatError::Field`] for unsupported languages.
    pub fn parse(s: &str) -> FormatResult<Self> {
        Ok(match s {
            "it" => Lang::It,
            "en" => Lang::En,
            "fr" => Lang::Fr,
            "de" => Lang::De,
            "es" => Lang::Es,
            "pt" => Lang::Pt,
            other => {
                return Err(FormatError::Field {
                    field: "lang",
                    message: format!(
                        "`{other}` is not one of it, en, fr, de, es, pt (other languages get no stemming)"
                    ),
                });
            }
        })
    }
}

/// A validity instant: a plain date or a UTC datetime (spec §1.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Validity(pub Date, pub Option<(u8, u8, u8)>);

impl Validity {
    /// Parse `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM:SSZ` (UTC only).
    ///
    /// # Errors
    /// [`FormatError::Field`] for anything else (including sub-second
    /// precision and non-UTC offsets, which the format forbids).
    pub fn parse(s: &str) -> FormatResult<Self> {
        let bad = |m: String| FormatError::Field {
            field: "validity",
            message: m,
        };
        if let Ok(d) = Date::parse(s, DATE_FORMAT) {
            return Ok(Validity(d, None));
        }
        let dt = OffsetDateTime::parse(s, &Rfc3339).map_err(|e| {
            bad(format!("`{s}` is neither a date nor an RFC 3339 UTC datetime: {e}"))
        })?;
        if dt.offset() != UtcOffset::UTC {
            return Err(bad(format!("`{s}`: only UTC datetimes are allowed")));
        }
        if dt.nanosecond() != 0 {
            return Err(bad(format!("`{s}`: sub-second precision is not allowed")));
        }
        Ok(Validity(dt.date(), Some((dt.hour(), dt.minute(), dt.second()))))
    }

    /// Wire/DB form: a date stays a date, a datetime keeps `Z`.
    pub fn to_db_string(&self) -> String {
        let d = format!(
            "{:04}-{:02}-{:02}",
            self.0.year(),
            u8::from(self.0.month()),
            self.0.day()
        );
        match self.1 {
            None => d,
            Some((h, m, s)) => format!("{d}T{h:02}:{m:02}:{s:02}Z"),
        }
    }

    /// Effective start instant (midnight for plain dates).
    pub fn start(&self) -> OffsetDateTime {
        self.0.midnight().assume_utc()
    }

    /// Effective exclusive end instant for a `valid_to` value: the next
    /// midnight for plain dates (the whole last day is valid), the instant
    /// itself for datetimes.
    pub fn end_exclusive(&self) -> OffsetDateTime {
        match self.1 {
            None => self
                .0
                .next_day()
                // 9999-12-31 has no next day: treat the end as far future by
                // using the last representable instant of that day.
                .unwrap_or(self.0)
                .with_time(time::Time::from_hms(23, 59, 59).unwrap_or(time::Time::MIDNIGHT))
                .assume_utc(),
            Some((h, m, s)) => self
                .0
                .with_time(
                    time::Time::from_hms(h, m, s).unwrap_or(time::Time::MIDNIGHT),
                )
                .assume_utc(),
        }
    }

    /// Is this validity in effect at `t`? (`from` side.)
    pub fn covers_from(&self, t: OffsetDateTime) -> bool {
        self.start() <= t
    }

    /// Is this validity end still in effect at `t`? (`to` side, exclusive
    /// with whole-day semantics for dates.)
    pub fn covers_to(&self, t: OffsetDateTime) -> bool {
        t < self.end_exclusive()
    }
}

impl std::fmt::Display for Validity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_db_string())
    }
}

/// A parsed timestamp (`*_at` fields): always UTC, second precision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct At(pub OffsetDateTime);

impl At {
    /// Parse `YYYY-MM-DDTHH:MM:SSZ`.
    ///
    /// # Errors
    /// [`FormatError::Field`] for non-UTC or sub-second values.
    pub fn parse(s: &str) -> FormatResult<Self> {
        let bad = |m: String| FormatError::Field {
            field: "timestamp",
            message: m,
        };
        let dt = OffsetDateTime::parse(s, &Rfc3339)
            .map_err(|e| bad(format!("`{s}` is not an RFC 3339 UTC datetime: {e}")))?;
        if dt.offset() != UtcOffset::UTC {
            return Err(bad(format!("`{s}`: only UTC datetimes are allowed")));
        }
        if dt.nanosecond() != 0 {
            return Err(bad(format!("`{s}`: sub-second precision is not allowed")));
        }
        Ok(Self(dt))
    }

    /// Wire form.
    pub fn to_db_string(&self) -> String {
        self.0.format(DATETIME_FORMAT).unwrap_or_default()
    }

    /// The inner instant.
    pub fn get(&self) -> OffsetDateTime {
        self.0
    }
}

impl std::fmt::Display for At {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_db_string())
    }
}

/// The note model: everything the format can represent.
#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    /// ULID.
    pub id: Ulid,
    /// `type`.
    pub note_type: NoteType,
    /// `title`, if any.
    pub title: Option<String>,
    /// Body, verbatim.
    pub body: String,
    /// Frontmatter tags (body tags are derived, see [`parse`]).
    pub tags: Vec<String>,
    /// `scope`.
    pub scope: Option<String>,
    /// `lang` override for stemming.
    pub lang: Option<Lang>,
    /// `source`.
    pub source: String,
    /// `confidence`.
    pub confidence: Confidence,
    /// `status`.
    pub status: Status,
    /// `valid_from`.
    pub valid_from: Option<Validity>,
    /// `valid_to`.
    pub valid_to: Option<Validity>,
    /// `supersedes`.
    pub supersedes: Vec<Ulid>,
    /// `created_at`.
    pub created_at: At,
    /// `updated_at`.
    pub updated_at: At,
    /// `x-*` extensions, in file order.
    pub extensions: Vec<(String, YamlValue)>,
}

/// The result of parsing a note file.
#[derive(Debug, Clone)]
pub struct ParsedNote {
    /// The note itself.
    pub note: Note,
    /// Body analysis (links, tags, relations).
    pub analysis: BodyAnalysis,
    /// Non-fatal warnings (import mode).
    pub warnings: Vec<String>,
}

impl Note {
    /// Display title: the title field or the first non-empty body line.
    pub fn display_title(&self) -> &str {
        if let Some(t) = self.title.as_deref() {
            return t;
        }
        self.body
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(str::trim)
            .unwrap_or("")
    }

    /// All tags: frontmatter plus body tags.
    pub fn all_tags(&self, analysis: &BodyAnalysis) -> Vec<String> {
        let mut tags = self.tags.clone();
        for t in &analysis.tags {
            if !tags.contains(t) {
                tags.push(t.clone());
            }
        }
        tags
    }

    /// The subject entity name, if this note has one (spec §3).
    pub fn subject_entity(&self) -> Option<String> {
        if !self.note_type.has_subject_entity() {
            return None;
        }
        let t = self.title.as_deref()?.trim();
        if t.is_empty() || t.contains(['\n', '\r', '.', ';', ':', '!', '?', '[', ']']) {
            return None;
        }
        Some(t.to_string())
    }

    /// Serialise to the canonical file form (spec §5).
    ///
    /// # Errors
    /// Only if a timestamp cannot be formatted (impossible for values that
    /// passed [`At::parse`]).
    pub fn serialize(&self) -> FormatResult<String> {
        let mut out = String::from("---\n");
        let line = |out: &mut String, k: &str, v: String| {
            let _ = writeln!(out, "{k}: {v}");
        };
        line(&mut out, "id", self.id.to_string());
        line(&mut out, "type", self.note_type.as_str().to_string());
        if let Some(t) = &self.title {
            line(&mut out, "title", YamlValue::emit_scalar(t));
        }
        if !self.tags.is_empty() {
            line(&mut out, "tags", emit_list(&self.tags));
        }
        if let Some(s) = &self.scope {
            line(&mut out, "scope", YamlValue::emit_scalar(s));
        }
        if let Some(l) = self.lang {
            line(&mut out, "lang", l.as_str().to_string());
        }
        line(&mut out, "source", YamlValue::emit_scalar(&self.source));
        line(&mut out, "confidence", self.confidence.as_str().to_string());
        line(&mut out, "status", self.status.as_str().to_string());
        if let Some(v) = &self.valid_from {
            line(&mut out, "valid_from", v.to_db_string());
        }
        if let Some(v) = &self.valid_to {
            line(&mut out, "valid_to", v.to_db_string());
        }
        if !self.supersedes.is_empty() {
            line(
                &mut out,
                "supersedes",
                emit_list(&self.supersedes.iter().map(|u| u.to_string()).collect::<Vec<_>>()),
            );
        }
        line(&mut out, "created_at", self.created_at.to_db_string());
        line(&mut out, "updated_at", self.updated_at.to_db_string());
        for (k, v) in &self.extensions {
            line(&mut out, k, v.emit_flow());
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        Ok(out)
    }

    /// Canonical JSON form (used in op payloads; keys are sorted by
    /// `serde_json`'s BTreeMaps, which is the canonical order).
    pub fn to_json(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("id".into(), self.id.to_string().into());
        m.insert("type".into(), self.note_type.as_str().into());
        if let Some(t) = &self.title {
            m.insert("title".into(), t.clone().into());
        }
        m.insert(
            "tags".into(),
            serde_json::Value::Array(self.tags.iter().map(|t| t.clone().into()).collect()),
        );
        if let Some(s) = &self.scope {
            m.insert("scope".into(), s.clone().into());
        }
        if let Some(l) = self.lang {
            m.insert("lang".into(), l.as_str().into());
        }
        m.insert("source".into(), self.source.clone().into());
        m.insert("confidence".into(), self.confidence.as_str().into());
        m.insert("status".into(), self.status.as_str().into());
        if let Some(v) = &self.valid_from {
            m.insert("valid_from".into(), v.to_db_string().into());
        }
        if let Some(v) = &self.valid_to {
            m.insert("valid_to".into(), v.to_db_string().into());
        }
        m.insert(
            "supersedes".into(),
            serde_json::Value::Array(
                self.supersedes.iter().map(|u| u.to_string().into()).collect(),
            ),
        );
        m.insert("created_at".into(), self.created_at.to_db_string().into());
        m.insert("updated_at".into(), self.updated_at.to_db_string().into());
        if !self.extensions.is_empty() {
            // An array of pairs keeps the file order across JSON round trips
            // (object keys would be sorted by serde_json).
            let x = serde_json::Value::Array(
                self.extensions
                    .iter()
                    .map(|(k, v)| serde_json::Value::Array(vec![
                        serde_json::Value::String(k.clone()),
                        v.to_json(),
                    ]))
                    .collect(),
            );
            m.insert("x".into(), x);
        }
        m.insert("body".into(), self.body.clone().into());
        serde_json::Value::Object(m)
    }

    /// Rebuild from canonical JSON (replay side).
    ///
    /// # Errors
    /// [`FormatError`] when the payload does not follow the model.
    pub fn from_json(v: &serde_json::Value) -> FormatResult<Self> {
        let obj = v.as_object().ok_or_else(|| bad_field("note", "expected a JSON object"))?;
        let get_str = |k: &str| -> FormatResult<&str> {
            obj.get(k).and_then(serde_json::Value::as_str).ok_or_else(|| {
                bad_field("note", format!("missing string field `{k}`"))
            })
        };
        let id = Ulid::from_string(get_str("id")?)
            .map_err(|_| bad_field("id", "not a ULID"))?;
        let note_type = NoteType::parse(get_str("type")?)?;
        let title = obj
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(String::from);
        let body = get_str("body")?.to_string();
        let tags = string_array(obj.get("tags"), "tags")?;
        let scope = obj
            .get("scope")
            .and_then(serde_json::Value::as_str)
            .map(String::from);
        let lang = match obj.get("lang").and_then(serde_json::Value::as_str) {
            Some(s) => Some(Lang::parse(s)?),
            None => None,
        };
        let source = get_str("source")?.to_string();
        let confidence = Confidence::parse(get_str("confidence")?)?;
        let status = Status::parse(get_str("status")?)?;
        let valid_from = match obj.get("valid_from").and_then(serde_json::Value::as_str) {
            Some(s) => Some(Validity::parse(s)?),
            None => None,
        };
        let valid_to = match obj.get("valid_to").and_then(serde_json::Value::as_str) {
            Some(s) => Some(Validity::parse(s)?),
            None => None,
        };
        let supersedes = string_array(obj.get("supersedes"), "supersedes")?
            .into_iter()
            .map(|s| {
                Ulid::from_string(&s).map_err(|_| bad_field("supersedes", format!("`{s}` is not a ULID")))
            })
            .collect::<FormatResult<Vec<_>>>()?;
        let created_at = At::parse(get_str("created_at")?)?;
        let updated_at = At::parse(get_str("updated_at")?)?;
        let mut extensions = Vec::new();
        if let Some(pairs) = obj.get("x").and_then(serde_json::Value::as_array) {
            for pair in pairs {
                let arr = pair.as_array().ok_or_else(|| {
                    bad_field("x", "expected an array of [key, value] pairs")
                })?;
                if arr.len() != 2 {
                    return Err(bad_field("x", "expected an array of [key, value] pairs"));
                }
                let k = arr[0].as_str().ok_or_else(|| bad_field("x", "keys must be strings"))?;
                extensions.push((k.to_string(), YamlValue::from_json(&arr[1])?));
            }
        }
        Ok(Note {
            id,
            note_type,
            title,
            body,
            tags,
            scope,
            lang,
            source,
            confidence,
            status,
            valid_from,
            valid_to,
            supersedes,
            created_at,
            updated_at,
            extensions,
        })
    }
}

fn string_array(v: Option<&serde_json::Value>, field: &'static str) -> FormatResult<Vec<String>> {
    let Some(v) = v else {
        return Ok(Vec::new());
    };
    let arr = v.as_array().ok_or_else(|| bad_field(field, "expected a list"))?;
    arr.iter()
        .map(|item| {
            item.as_str()
                .map(String::from)
                .ok_or_else(|| bad_field(field, "expected strings"))
        })
        .collect()
}

fn emit_list(items: &[String]) -> String {
    let mut out = String::from("[");
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&YamlValue::emit_scalar(item));
    }
    out.push(']');
    out
}

/// Parse a note file.
///
/// # Errors
/// See [`FormatError`]; every violation of spec §1 is a readable error.
pub fn parse(text: &str, mode: ParseMode) -> FormatResult<ParsedNote> {
    let (header, body) = split_frontmatter(text)?;
    let entries = yaml::parse_mapping(&header)?;
    let mut warnings = Vec::new();

    let mut id = None;
    let mut note_type = None;
    let mut title = None;
    let mut tags: Option<Vec<String>> = None;
    let mut scope = None;
    let mut lang = None;
    let mut source = None;
    let mut confidence = None;
    let mut status = None;
    let mut valid_from = None;
    let mut valid_to = None;
    let mut supersedes: Option<Vec<Ulid>> = None;
    let mut created_at = None;
    let mut updated_at = None;
    let mut extensions = Vec::new();

    for (k, v) in entries {
        match k.as_str() {
            "id" => {
                let s = v.as_str().ok_or_else(|| bad_field("id", "expected a string"))?;
                let u = Ulid::from_string(s)
                    .map_err(|_| bad_field("id", format!("`{s}` is not a ULID")))?;
                if u.to_string() != s {
                    return Err(bad_field("id", format!("`{s}` is not canonical (uppercase)")));
                }
                id = Some(u);
            }
            "type" => {
                note_type = Some(NoteType::parse(
                    v.as_str().ok_or_else(|| bad_field("type", "expected a string"))?,
                )?)
            }
            "title" => {
                let s = v.as_str().ok_or_else(|| bad_field("title", "expected a string"))?;
                if s.contains(['\n', '\r']) {
                    return Err(bad_field("title", "must be a single line"));
                }
                title = Some(s.to_string());
            }
            "tags" => {
                let YamlValue::Seq(items) = &v else {
                    return Err(bad_field("tags", "expected a list"));
                };
                let mut list = Vec::with_capacity(items.len());
                for item in items {
                    let s = item.as_str().ok_or_else(|| bad_field("tags", "expected strings"))?;
                    validate_tag_name(s)?;
                    list.push(s.to_string());
                }
                tags = Some(list);
            }
            "scope" => {
                scope = Some(
                    v.as_str()
                        .ok_or_else(|| bad_field("scope", "expected a string"))?
                        .to_string(),
                )
            }
            "lang" => {
                lang = Some(Lang::parse(
                    v.as_str().ok_or_else(|| bad_field("lang", "expected a string"))?,
                )?)
            }
            "source" => {
                source = Some(
                    v.as_str()
                        .ok_or_else(|| bad_field("source", "expected a string"))?
                        .to_string(),
                )
            }
            "confidence" => {
                confidence = Some(Confidence::parse(
                    v.as_str()
                        .ok_or_else(|| bad_field("confidence", "expected a string"))?,
                )?)
            }
            "status" => {
                status = Some(Status::parse(
                    v.as_str()
                        .ok_or_else(|| bad_field("status", "expected a string"))?,
                )?)
            }
            "valid_from" => {
                valid_from = Some(Validity::parse(
                    v.as_str()
                        .ok_or_else(|| bad_field("valid_from", "expected a date or datetime"))?,
                )?)
            }
            "valid_to" => {
                valid_to = Some(Validity::parse(
                    v.as_str()
                        .ok_or_else(|| bad_field("valid_to", "expected a date or datetime"))?,
                )?)
            }
            "supersedes" => {
                let YamlValue::Seq(items) = &v else {
                    return Err(bad_field("supersedes", "expected a list"));
                };
                let mut list = Vec::with_capacity(items.len());
                for item in items {
                    let s = item
                        .as_str()
                        .ok_or_else(|| bad_field("supersedes", "expected strings"))?;
                    list.push(Ulid::from_string(s)
                        .map_err(|_| bad_field("supersedes", format!("`{s}` is not a ULID")))?);
                }
                supersedes = Some(list);
            }
            "created_at" => {
                created_at = Some(At::parse(
                    v.as_str()
                        .ok_or_else(|| bad_field("created_at", "expected a UTC datetime"))?,
                )?)
            }
            "updated_at" => {
                updated_at = Some(At::parse(
                    v.as_str()
                        .ok_or_else(|| bad_field("updated_at", "expected a UTC datetime"))?,
                )?)
            }
            other => {
                if other.starts_with("x-") {
                    extensions.push((other.to_string(), v));
                } else {
                    match mode {
                        ParseMode::Strict => {
                            return Err(FormatError::UnknownKey(other.to_string()))
                        }
                        ParseMode::Import => {
                            warnings.push(format!("unknown key `{other}` ignored"))
                        }
                    }
                }
            }
        }
    }

    let confidence = confidence.ok_or(FormatError::MissingField("confidence"))?;
    let status = match status {
        Some(s) => {
            if confidence == Confidence::External && s != Status::Quarantined {
                return Err(bad_field(
                    "status",
                    "an `external` note can only be `quarantined`",
                ));
            }
            s
        }
        None if confidence == Confidence::External => Status::Quarantined,
        None => Status::Active,
    };
    let note = Note {
        id: id.ok_or(FormatError::MissingField("id"))?,
        note_type: note_type.ok_or(FormatError::MissingField("type"))?,
        title,
        body,
        tags: tags.unwrap_or_default(),
        scope,
        lang,
        source: source.ok_or(FormatError::MissingField("source"))?,
        confidence,
        status,
        valid_from,
        valid_to,
        supersedes: supersedes.unwrap_or_default(),
        created_at: created_at.ok_or(FormatError::MissingField("created_at"))?,
        updated_at: updated_at.ok_or(FormatError::MissingField("updated_at"))?,
        extensions,
    };
    let analysis = body::analyze(&note.body)?;
    for tag in &analysis.tags {
        validate_tag_name(tag)?;
    }
    Ok(ParsedNote {
        note,
        analysis,
        warnings,
    })
}

fn bad_field(field: &'static str, message: impl Into<String>) -> FormatError {
    FormatError::Field {
        field,
        message: message.into(),
    }
}

fn validate_tag_name(tag: &str) -> FormatResult<()> {
    let ok = !tag.is_empty()
        && tag.split('/').all(|seg| {
            !seg.is_empty() && seg.chars().all(|c| c.is_alphanumeric() || c == '_')
        });
    if ok {
        Ok(())
    } else {
        Err(bad_field(
            "tags",
            format!(
                "`{tag}` is not a valid tag name (segments of letters, digits, `_`, joined by `/`)"
            ),
        ))
    }
}

/// Split `---\n…\n---\n<body>`; returns `(header, body)`.
fn split_frontmatter(text: &str) -> FormatResult<(String, String)> {
    if !text.starts_with("---\n") {
        return Err(FormatError::NoFrontmatter);
    }
    let rest = &text[4..];
    let Some(idx) = rest.find("\n---\n").map(|i| i + 1) else {
        return Err(FormatError::Yaml(
            "frontmatter is not closed by a `---` line".into(),
        ));
    };
    // `rest[..idx]` is the header; the closing delimiter `---\n` occupies
    // `rest[idx..idx+4]`; the body starts right after it, verbatim.
    let header = rest[..idx].to_string();
    let body = rest[idx + 4..].to_string();
    Ok((header, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_text() -> String {
        "---\n\
         id: 01JC8Z6Q2W8M3N5V4T7Y9R0P1S\n\
         type: fact\n\
         source: user\n\
         confidence: stated\n\
         status: active\n\
         created_at: 2026-09-03T10:12:00Z\n\
         updated_at: 2026-09-03T10:12:00Z\n\
         ---\n\
         Fabio abita a [[Milano]].\n"
            .to_string()
    }

    #[test]
    fn minimal_round_trip() {
        let p = parse(&minimal_text(), ParseMode::Strict).unwrap(/* test-only */);
        assert_eq!(p.note.id.to_string(), "01JC8Z6Q2W8M3N5V4T7Y9R0P1S");
        assert_eq!(p.analysis.links, vec!["Milano"]);
        let s = p.note.serialize().unwrap(/* test-only */);
        assert_eq!(s, minimal_text());
        let p2 = parse(&s, ParseMode::Strict).unwrap(/* test-only */);
        assert_eq!(p2.note, p.note);
        let s2 = p2.note.serialize().unwrap(/* test-only */);
        assert_eq!(s, s2);
    }

    #[test]
    fn strict_rejects_unknown_key_import_warns() {
        let text = minimal_text().replace("source: user", "source: user\nfavourite: tè");
        assert!(matches!(
            parse(&text, ParseMode::Strict),
            Err(FormatError::UnknownKey(k)) if k == "favourite"
        ));
        let p = parse(&text, ParseMode::Import).unwrap(/* test-only */);
        assert_eq!(p.warnings.len(), 1);
        assert!(!p.note.serialize().unwrap(/* test-only */).contains("favourite:"));
    }

    #[test]
    fn external_forces_quarantine() {
        let text = minimal_text()
            .replace("confidence: stated", "confidence: external")
            .replace("source: user", "source: import:web")
            .replace("status: active\n", "");
        let p = parse(&text, ParseMode::Strict).unwrap(/* test-only */);
        assert_eq!(p.note.status, Status::Quarantined);
    }

    #[test]
    fn external_with_active_is_rejected() {
        let text = "---\n\
             id: 01JC8Z6Q2W8M3N5V4T7Y9R0P1S\n\
             type: fact\n\
             source: import:web\n\
             confidence: external\n\
             status: active\n\
             created_at: 2026-09-03T10:12:00Z\n\
             updated_at: 2026-09-03T10:12:00Z\n\
             ---\n\
             No.\n";
        assert!(parse(text, ParseMode::Strict).is_err());
    }

    #[test]
    fn validity_semantics() {
        let d = Validity::parse("2026-09-01").unwrap(/* test-only */);
        assert_eq!(d.to_db_string(), "2026-09-01");
        let noon = Date::from_calendar_date(2026, time::Month::September, 1)
            .unwrap(/* test-only */)
            .with_time(time::Time::from_hms(12, 0, 0).unwrap(/* test-only */))
            .assume_utc();
        assert!(d.covers_from(noon));
        assert!(d.covers_to(noon)); // the whole day is valid
        let dt = Validity::parse("2026-09-01T12:00:00Z").unwrap(/* test-only */);
        assert!(!dt.covers_to(noon + time::Duration::hours(1)));
        assert!(Validity::parse("2026-09-01T12:00:00+02:00").is_err());
        assert!(Validity::parse("2026-09-01T12:00:00.5Z").is_err());
    }

    #[test]
    fn json_round_trip_with_extensions() {
        let text =
            minimal_text().replace("status: active", "status: active\nx-homun: {bot: casa}\nx-n: 7");
        let p = parse(&text, ParseMode::Strict).unwrap(/* test-only */);
        let j = p.note.to_json();
        let back = Note::from_json(&j).unwrap(/* test-only */);
        assert_eq!(back, p.note);
    }

    #[test]
    fn subject_entity_rules() {
        let mut note = parse(&minimal_text(), ParseMode::Strict)
            .unwrap(/* test-only */)
            .note;
        assert_eq!(note.subject_entity(), None); // fact
        note.note_type = NoteType::Person;
        note.title = Some("Fabio".into());
        assert_eq!(note.subject_entity().as_deref(), Some("Fabio"));
        note.title = Some("Fabio abita a Roma.".into());
        assert_eq!(note.subject_entity(), None);
    }

    #[test]
    fn no_frontmatter_is_rejected() {
        assert!(matches!(
            parse("plain text\n", ParseMode::Strict),
            Err(FormatError::NoFrontmatter)
        ));
    }

    #[test]
    fn empty_body_round_trips() {
        let text = minimal_text().replace("Fabio abita a [[Milano]].\n", "");
        let p = parse(&text, ParseMode::Strict).unwrap(/* test-only */);
        assert_eq!(p.note.body, "");
        assert_eq!(p.note.serialize().unwrap(/* test-only */), text);
    }

    #[test]
    fn body_keeps_verbatim_bytes() {
        let weird = "riga con\ttab e  doppi spazi\n\n\nultima senza newline";
        let text = format!("{}{}", minimal_text(), weird);
        let p = parse(&text, ParseMode::Strict).unwrap(/* test-only */);
        assert_eq!(p.note.body, format!("Fabio abita a [[Milano]].\n{weird}"));
    }

    #[test]
    fn bad_values_are_readable_errors() {
        assert!(parse(
            &minimal_text().replace("type: fact", "type: idea"),
            ParseMode::Strict
        )
        .is_err());
        assert!(parse(
            &minimal_text().replace("id: 01JC8Z6Q2W8M3N5V4T7Y9R0P1S", "id: corto"),
            ParseMode::Strict
        )
        .is_err());
        assert!(parse(
            &minimal_text().replace("status: active", "tags: [\"tag con spazi\"]"),
            ParseMode::Strict
        )
        .is_err());
        assert!(parse(
            &minimal_text().replace("updated_at: 2026-09-03T10:12:00Z", "updated_at: 2026-09-03"),
            ParseMode::Strict
        )
        .is_err());
    }
}
