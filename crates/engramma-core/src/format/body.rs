// SPDX-License-Identifier: Apache-2.0

//! Body analysis with CommonMark events (`pulldown-cmark`): wiki links, body
//! tags and `## Relations` entries. Code blocks, inline code and HTML never
//! contribute; tags are additionally excluded from headings (spec §2–§4).

use std::collections::BTreeSet;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use regex::Regex;

use crate::error::{FormatError, FormatResult};
use crate::format::Validity;

/// A typed relation declared in `## Relations`.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationDecl {
    /// Relation name (`lives_in`).
    pub rel: String,
    /// Destination entity name.
    pub dst: String,
    /// Optional validity start (`from DATE`).
    pub from: Option<Validity>,
    /// Optional validity end (`to DATE`).
    pub to: Option<Validity>,
}

/// Everything the body declares, extracted once at parse time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BodyAnalysis {
    /// Distinct `[[Name]]` targets outside the Relations section, in order of
    /// first appearance.
    pub links: Vec<String>,
    /// Distinct `#tag` names from body text (not headings, not code).
    pub tags: Vec<String>,
    /// Relations from the `## Relations` section, in order.
    pub relations: Vec<RelationDecl>,
}

fn link_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[\[([^\]|]+)(?:\|([^\]]*))?\]\]").unwrap(/* static regex, valid by construction */))
}

fn tag_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?:^|\s)#([\p{L}\p{N}_]+(?:/[\p{L}\p{N}_]+)*)")
            .unwrap(/* static regex, valid by construction */)
    })
}

fn relation_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"^([a-z][a-z0-9_]*)::\s*\[\[([^\]|]+)(?:\|[^\]]*)?\]\]\s*(?:\(from\s+(\S+)(?:\s+to\s+(\S+))?\)|\(to\s+(\S+)\))?\s*$",
        ).unwrap(/* static regex, valid by construction */)
    })
}

/// Walk the events of a body and collect links, tags and relations.
///
/// `pulldown-cmark` splits `[[Milano]]` into several `Text` events, so text is
/// accumulated into runs and scanned when a run ends (any non-text event).
/// Runs inside code blocks are dropped; runs in headings contribute links but
/// never tags; text in `## Relations` items becomes relation lines.
///
/// # Errors
/// [`FormatError::RelationLine`] when a Relations item does not follow the
/// spec syntax (including invalid dates); [`FormatError::Field`] for invalid
/// link names.
pub fn analyze(body: &str) -> FormatResult<BodyAnalysis> {
    let mut out = BodyAnalysis::default();
    let mut seen_links = BTreeSet::new();
    let mut seen_tags = BTreeSet::new();

    let mut in_code_block = false;
    let mut in_heading = false;
    let mut heading_text = String::new();
    let mut heading_level = HeadingLevel::H1;
    let mut in_relations = false;
    let mut in_item = false;
    let mut item_text = String::new();
    let mut run = String::new();
    // Last character emitted in running text, to enforce the "preceded by
    // whitespace or start of text" rule for tags across runs.
    let mut last_char: Option<char> = None;

    for ev in Parser::new_ext(body, Options::empty()) {
        match ev {
            Event::Text(t) => {
                if in_code_block {
                    continue;
                }
                if in_heading {
                    heading_text.push_str(&t);
                } else if in_relations && in_item {
                    item_text.push_str(&t);
                } else {
                    run.push_str(&t);
                }
            }
            Event::Code(_) => {
                flush_run(&mut out, &mut seen_links, &mut seen_tags, &mut run, &mut last_char)?;
                last_char = Some('`');
            }
            Event::Html(_) | Event::InlineHtml(_) | Event::SoftBreak | Event::HardBreak => {
                flush_run(&mut out, &mut seen_links, &mut seen_tags, &mut run, &mut last_char)?;
                if matches!(ev, Event::SoftBreak | Event::HardBreak) {
                    last_char = Some('\n');
                }
            }
            Event::Start(Tag::CodeBlock(_)) => {
                flush_run(&mut out, &mut seen_links, &mut seen_tags, &mut run, &mut last_char)?;
                in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                flush_run(&mut out, &mut seen_links, &mut seen_tags, &mut run, &mut last_char)?;
                in_code_block = false;
            }
            Event::Start(Tag::Heading { level, .. }) => {
                flush_run(&mut out, &mut seen_links, &mut seen_tags, &mut run, &mut last_char)?;
                in_heading = true;
                heading_text.clear();
                heading_level = level;
                // Any later heading closes the Relations section.
                in_relations = false;
            }
            Event::End(TagEnd::Heading(_)) => {
                flush_run(&mut out, &mut seen_links, &mut seen_tags, &mut run, &mut last_char)?;
                in_heading = false;
                if heading_level <= HeadingLevel::H2 && heading_text.trim() == "Relations" {
                    in_relations = true;
                }
                // Links are recognised in heading text too (tags are not).
                scan_links(&heading_text, &mut seen_links, &mut out.links)?;
            }
            Event::Start(Tag::Paragraph) | Event::Start(Tag::List(_)) => {
                flush_run(&mut out, &mut seen_links, &mut seen_tags, &mut run, &mut last_char)?;
                last_char = None;
            }
            Event::Start(Tag::Item) => {
                flush_run(&mut out, &mut seen_links, &mut seen_tags, &mut run, &mut last_char)?;
                in_item = true;
                item_text.clear();
                last_char = None;
            }
            Event::End(TagEnd::Item) => {
                flush_run(&mut out, &mut seen_links, &mut seen_tags, &mut run, &mut last_char)?;
                if in_relations {
                    out.relations.push(parse_relation_line(&item_text)?);
                }
                in_item = false;
            }
            _ => {
                flush_run(&mut out, &mut seen_links, &mut seen_tags, &mut run, &mut last_char)?;
            }
        }
    }
    flush_run(&mut out, &mut seen_links, &mut seen_tags, &mut run, &mut last_char)?;
    Ok(out)
}

/// Scan one accumulated run for links and tags, then clear it.
fn flush_run(
    out: &mut BodyAnalysis,
    seen_links: &mut BTreeSet<String>,
    seen_tags: &mut BTreeSet<String>,
    run: &mut String,
    last_char: &mut Option<char>,
) -> FormatResult<()> {
    if run.is_empty() {
        return Ok(());
    }
    let masked = scan_links(run, seen_links, &mut out.links)?;
    for cap in tag_re().captures_iter(&masked) {
        let preceded_ok = match cap.get(1) {
            Some(m) if m.start() == 0 => last_char.map_or(true, char::is_whitespace),
            _ => true,
        };
        if preceded_ok {
            let tag = cap.get(1).map(|m| m.as_str().to_string()).unwrap_or_default();
            if seen_tags.insert(tag.clone()) {
                out.tags.push(tag);
            }
        }
    }
    *last_char = run.chars().next_back();
    run.clear();
    Ok(())
}

/// Collect `[[Name]]` links from a text; returns the text with the link spans
/// masked (same-length spaces, so ranges stay valid for the caller).
fn scan_links(
    text: &str,
    seen: &mut BTreeSet<String>,
    links: &mut Vec<String>,
) -> FormatResult<String> {
    let mut masked = text.to_string();
    
    for cap in link_re().captures_iter(text) {
        let name = cap
            .get(1)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        if name.is_empty() || name.contains(['[', ']', '|']) {
            return Err(FormatError::Field {
                field: "body",
                message: format!("invalid link name `{name}`"),
            });
        }
        if seen.insert(name.clone()) {
            links.push(name);
        }
        if let Some(m) = cap.get(0) {
            let spaces = " ".repeat(m.as_str().len());
            masked.replace_range(m.range(), &spaces);
        }
    }
    Ok(masked)
}


/// Parse one `- rel:: [[Dst]] (from … to …)` line.
fn parse_relation_line(line: &str) -> FormatResult<RelationDecl> {
    let line = line.trim();
    let Some(cap) = relation_re().captures(line) else {
        return Err(FormatError::RelationLine(
            line.to_string(),
            "expected `- name:: [[Entity]]` with optional `(from DATE to DATE)`".into(),
        ));
    };
    let g = |i: usize| cap.get(i).map(|m| m.as_str().to_string());
    let from = match g(3) {
        Some(s) => Some(Validity::parse(&s).map_err(|e| {
            FormatError::RelationLine(line.to_string(), format!("invalid `from` date: {e}"))
        })?),
        None => None,
    };
    let to = match g(4).or_else(|| g(5)) {
        Some(s) => Some(Validity::parse(&s).map_err(|e| {
            FormatError::RelationLine(line.to_string(), format!("invalid `to` date: {e}"))
        })?),
        None => None,
    };
    Ok(RelationDecl {
        rel: g(1).unwrap_or_default(),
        dst: g(2).unwrap_or_default().trim().to_string(),
        from,
        to,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_tags_and_relations() {
        let body = "Fabio abita a [[Milano]] con [[Giulia|G.]] dal 2026.\n\
                    Vedi #casa/milano e #trasloco.\n\
                    Codice: `[[No]]` e:\n\
                    ```text\n[[Neppure]] #no\n```\n\
                    # Titolo con #dentro\n\
                    \n\
                    ## Relations\n\
                    - lives_in:: [[Milano]] (from 2026-09-01)\n\
                    - partner_of:: [[Giulia]]\n\
                    - worked_at:: [[Acme]] (from 2020-01-06 to 2024-12-31)\n";
        let a = analyze(body).unwrap(/* test-only invariant */);
        assert_eq!(a.links, vec!["Milano".to_string(), "Giulia".to_string()]);
        assert_eq!(
            a.tags,
            vec!["casa/milano".to_string(), "trasloco".to_string()]
        );
        assert_eq!(a.relations.len(), 3);
        assert_eq!(a.relations[0].rel, "lives_in");
        assert_eq!(a.relations[0].dst, "Milano");
        assert!(a.relations[0].from.is_some());
        assert!(a.relations[0].to.is_none());
        assert_eq!(
            a.relations[2]
                .to
                .as_ref()
                .map(|t| t.to_db_string()),
            Some("2024-12-31".to_string())
        );
    }

    #[test]
    fn bad_relation_line_is_an_error() {
        assert!(analyze("## Relations\n- vive a Milano\n").is_err());
        assert!(analyze("## Relations\n- lives_in:: [[X]] (from non-una-data)\n").is_err());
    }

    #[test]
    fn tag_boundary_rules() {
        let a = analyze("parola#notag ma #ok\n").unwrap(/* test-only invariant */);
        assert_eq!(a.tags, vec!["ok".to_string()]);
        let a2 = analyze("**grassetto**\n#dopo_a_capo\n").unwrap(/* test-only invariant */);
        assert_eq!(a2.tags, vec!["dopo_a_capo".to_string()]);
    }

    #[test]
    fn relations_section_ends_at_next_heading() {
        let a =
            analyze("## Relations\n- a:: [[B]]\n\n## Altro\n- c:: [[D]]\n").unwrap(/* test-only */);
        assert_eq!(a.relations.len(), 1);
        assert_eq!(a.links, vec!["D".to_string()]);
    }

    #[test]
    fn relations_only_at_level_one_or_two() {
        assert!(analyze("### Relations\n- a:: [[B]]\n").unwrap(/* test-only */).relations.is_empty());
    }
}
