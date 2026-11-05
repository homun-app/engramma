// SPDX-License-Identifier: Apache-2.0

//! Text normalisation for the FTS index (`body_norm`) and for queries
//! (DESIGN §6.1): lowercase → strip Latin diacritics by hand → Snowball
//! stemming in the note's language.

use rust_stemmers::{Algorithm, Stemmer};
use whatlang::Lang as WhatLang;

use crate::format::Lang;

/// Detect the stemming language from a text (`whatlang`); only the six
/// languages with a bundled stemmer are recognised, everything else gets no
/// stemming.
pub fn detect_lang(text: &str) -> Option<Lang> {
    let info = whatlang::detect(text)?;
    match info.lang() {
        WhatLang::Ita => Some(Lang::It),
        WhatLang::Eng => Some(Lang::En),
        WhatLang::Fra => Some(Lang::Fr),
        WhatLang::Deu => Some(Lang::De),
        WhatLang::Spa => Some(Lang::Es),
        WhatLang::Por => Some(Lang::Pt),
        _ => None,
    }
}

fn algorithm(lang: Lang) -> Algorithm {
    match lang {
        Lang::It => Algorithm::Italian,
        Lang::En => Algorithm::English,
        Lang::Fr => Algorithm::French,
        Lang::De => Algorithm::German,
        Lang::Es => Algorithm::Spanish,
        Lang::Pt => Algorithm::Portuguese,
    }
}

/// Normalise `text` (already lowercase-agnostic) for the index: lowercase,
/// strip Latin diacritics, stem each token in `lang` (no stemming when the
/// language is unknown).
pub fn normalize(text: &str, lang: Option<Lang>) -> String {
    let lowered = text.to_lowercase();
    let folded = strip_diacritics(&lowered);
    let stemmer = lang.map(|l| Stemmer::create(algorithm(l)));
    let mut out = String::with_capacity(folded.len());
    let mut first = true;
    for token in folded.split(|c: char| !c.is_alphanumeric()) {
        let token = token.trim_matches(|c: char| !c.is_alphanumeric());
        if token.is_empty() {
            continue;
        }
        if !first {
            out.push(' ');
        }
        first = false;
        match &stemmer {
            Some(s) => out.push_str(&s.stem(token)),
            None => out.push_str(token),
        }
    }
    out
}

/// Normalise a title+body pair for `body_norm`: the title is prepended so the
/// stemmed index covers it too (the FTS `title` column keeps the raw text and
/// is tokenised without stemming).
pub fn normalize_for_index(title: Option<&str>, body: &str, lang: Option<Lang>) -> String {
    match title {
        Some(t) if !t.trim().is_empty() => normalize(&format!("{t}\n{body}"), lang),
        _ => normalize(body, lang),
    }
}

/// Query terms for the FTS index: the stems the index may hold for each query
/// token. With an explicit language only its stem is used; without one, the
/// detection runs on the query and — when it does not land on a supported
/// language (queries are short) — the token is searched in its raw form and in
/// the stem of every supported language (deduplicated). Recall-oriented: BM25
/// still ranks better-fitting documents first.
/// Italian and English question words and fillers: they match almost every
/// document and carry no signal, so they are dropped from queries.
const STOPWORDS: &[&str] = &[
    // italiano
    "dove", "chi", "che", "cosa", "quando", "quale", "quali", "quanto", "quanti", "quanta",
    "quante", "come", "perche", "quale", "e", "ed", "o", "ma", "non", "si", "no", "io", "tu",
    "lui", "lei", "noi", "voi", "loro", "il", "lo", "la", "i", "gli", "le", "un", "uno", "una",
    "di", "a", "al", "allo", "alla", "ai", "agli", "alle", "da", "dal", "dallo", "dalla", "dai",
    "dagli", "dalle", "in", "nel", "nello", "nella", "nei", "negli", "nelle", "con", "su", "sul",
    "sullo", "sulla", "sui", "sugli", "sulle", "per", "tra", "fra", "del", "dello", "della",
    "dei", "degli", "delle", "era", "erano", "ero", "sono", "stato", "stata", "stati",
    "state", "essere", "fa", "fanno", "fatto", "ha", "hai", "hanno", "ho", "sia", "va",
    "vanno", "questo", "questa", "questi", "queste", "quello", "quella", "quelli",
    "quelle", "cui", "ne", "ci", "vi", "mi", "ti", "se", "anche", "solo", "molto", "poco",
    "piu", "meno", "ogni", "tutti", "tutto", "tutta", "tutte", "alcuni", "alcuno",
    "nessuno", "altro", "altra", "altri", "altre", "senza", "verso", "cio",
    "adesso", "ancora", "prima", "dopo", "sempre", "mai", "poi", "gia", "qui", "la",
    // inglese
    "the", "a", "an", "of", "in", "on", "at", "to", "for", "with", "by", "is", "are", "was",
    "were", "be", "been", "who", "whats", "what", "where", "when", "why", "how", "does", "do",
    "did", "his", "her", "their", "its", "and", "or", "but", "not", "now", "still", "then",
];

pub fn query_terms(text: &str, lang: Option<Lang>) -> Vec<String> {
    let lowered = text.to_lowercase();
    let tokens = tokenize(&lowered);
    let mut out: Vec<String> = Vec::new();
    let mut push = |t: String| {
        if !out.contains(&t) {
            out.push(t);
        }
    };
    let detected = lang.or_else(|| detect_lang(text));
    for token in tokens {
        // Stopwords are dropped both raw and after stemming ("è" stems to
        // "e" in Italian: both spellings must go).
        if STOPWORDS.contains(&token) {
            continue;
        }
        match detected {
            Some(l) => {
                let stem = normalize(token, Some(l));
                if !STOPWORDS.contains(&stem.as_str()) {
                    push(stem);
                }
            }
            None => {
                let mut stems: Vec<String> = vec![token.to_string()];
                for l in [Lang::It, Lang::En, Lang::Fr, Lang::De, Lang::Es, Lang::Pt] {
                    stems.push(normalize(token, Some(l)));
                }
                for stem in stems {
                    if !STOPWORDS.contains(&stem.as_str()) {
                        push(stem);
                    }
                }
            }
        }
    }
    out
}

fn tokenize(lowered: &str) -> Vec<&str> {
    lowered
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Strip Latin diacritics with a hand-written table (Latin-1 Supplement and
/// Latin Extended-A; `×` and `÷` are not letters and stay). This is the
/// "NFKD senza diacritici" step of §6.1, written without a Unicode crate.
pub fn strip_diacritics(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            // Latin-1 Supplement
            'À'..='Þ' | 'à'..='þ' if !matches!(c, '×' | '÷') => {
                // Ranges exclude × (00D7) and ÷ (00F7) via the guard; map the
                // rest through the table below.
                out.push_str(&latin1(c));
            }
            // Latin Extended-A
            '\u{0100}'..='\u{017F}' => out.push_str(&latin_ext_a(c)),
            _ => out.push(c),
        }
    }
    out
}

fn latin1(c: char) -> &'static str {
    match c {
        'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => "a",
        'Æ' | 'æ' => "ae",
        'Ç' | 'ç' => "c",
        'È' | 'É' | 'Ê' | 'Ë' | 'è' | 'é' | 'ê' | 'ë' => "e",
        'Ì' | 'Í' | 'Î' | 'Ï' | 'ì' | 'í' | 'î' | 'ï' => "i",
        'Ð' | 'ð' => "d",
        'Ñ' | 'ñ' => "n",
        'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'ò' | 'ó' | 'ô' | 'õ' | 'ö' => "o",
        'Ø' | 'ø' => "o",
        'Ù' | 'Ú' | 'Û' | 'Ü' | 'ù' | 'ú' | 'û' | 'ü' => "u",
        'Ý' | 'ÿ' | 'ý' => "y",
        'Þ' | 'þ' => "th",
        'ß' => "ss",
        _ => "?",
    }
}

fn latin_ext_a(c: char) -> &'static str {
    match c {
        'Ā' | 'ā' | 'Ă' | 'ă' | 'Ą' | 'ą' => "a",
        'Ć' | 'ć' | 'Ĉ' | 'ĉ' | 'Ċ' | 'ċ' | 'Č' | 'č' => "c",
        'Ď' | 'ď' | 'Đ' | 'đ' => "d",
        'Ē' | 'ē' | 'Ĕ' | 'ĕ' | 'Ė' | 'ė' | 'Ę' | 'ę' | 'Ě' | 'ě' => "e",
        'Ĝ' | 'ĝ' | 'Ğ' | 'ğ' | 'Ġ' | 'ġ' | 'Ģ' | 'ģ' => "g",
        'Ĥ' | 'ĥ' | 'Ħ' | 'ħ' => "h",
        'Ĩ' | 'ĩ' | 'Ī' | 'ī' | 'Ĭ' | 'ĭ' | 'Į' | 'į' | 'İ' | 'ı' => "i",
        'Ĳ' | 'ĳ' => "ij",
        'Ĵ' | 'ĵ' => "j",
        'Ķ' | 'ķ' | 'ĸ' => "k",
        'Ĺ' | 'ĺ' | 'Ļ' | 'ļ' | 'Ľ' | 'ľ' | 'Ŀ' | 'ŀ' | 'Ł' | 'ł' => "l",
        'Ń' | 'ń' | 'Ņ' | 'ņ' | 'Ň' | 'ň' | 'ŉ' => "n",
        'Ō' | 'ō' | 'Ŏ' | 'ŏ' | 'Ő' | 'ő' => "o",
        'Œ' | 'œ' => "oe",
        'Ŕ' | 'ŕ' | 'Ŗ' | 'ŗ' | 'Ř' | 'ř' => "r",
        'Ś' | 'ś' | 'Ŝ' | 'ŝ' | 'Ş' | 'ş' | 'Š' | 'š' => "s",
        'Ţ' | 'ţ' | 'Ť' | 'ť' | 'Ŧ' | 'ŧ' => "t",
        'Ũ' | 'ũ' | 'Ū' | 'ū' | 'Ŭ' | 'ŭ' | 'Ů' | 'ů' | 'Ű' | 'ű' | 'Ų' | 'ų' => "u",
        'Ŵ' | 'ŵ' => "w",
        'Ŷ' | 'ŷ' => "y",
        'Ÿ' => "y",
        'Ź' | 'ź' | 'Ż' | 'ż' | 'Ž' | 'ž' => "z",
        'Ƒ' | 'ƒ' => "f",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diacritics() {
        assert_eq!(strip_diacritics("città perù Übung déjà"), "citta peru ubung deja");
        assert_eq!(strip_diacritics("Łódź Œuvre"), "lodz oeuvre");
        assert_eq!(strip_diacritics("2 × 3 ÷ 2"), "2 × 3 ÷ 2");
    }

    #[test]
    fn italian_stemming_golds() {
        let a = normalize("traslocato", Some(Lang::It));
        let b = normalize("trasloco", Some(Lang::It));
        let c = normalize("traslochi", Some(Lang::It));
        assert_eq!(a, b);
        assert_eq!(b, c);
        let citta = normalize("città", Some(Lang::It));
        assert_eq!(citta, normalize("citta", Some(Lang::It)));
    }

    #[test]
    fn no_stemming_for_unknown_language() {
        let s = normalize("Привет мир", None);
        assert_eq!(s, "привет мир");
    }

    #[test]
    fn tokens_split_on_punctuation() {
        assert_eq!(
            normalize("Il trasloco, da Roma a Milano!", Some(Lang::It)),
            normalize("il trasloco da roma a milano", Some(Lang::It))
        );
    }

    #[test]
    fn detection() {
        assert_eq!(
            detect_lang("Fabio abita a Milano con Giulia, il trasloco è previsto per settembre"),
            Some(Lang::It)
        );
        assert_eq!(
            detect_lang(
                "Giulia lavora come architetta presso uno studio di progettazione a Milano"
            ),
            Some(Lang::It)
        );
        assert_eq!(detect_lang("The quick brown fox jumps over the lazy dog"), Some(Lang::En));
        // Unsupported languages get no stemming.
        assert_eq!(detect_lang("Привет, как дела сегодня"), None);
    }

    #[test]
    fn query_terms_include_all_languages_when_unknown() {
        // A word in an unsupported language (Russian): raw token plus the
        // stems of every supported language.
        let terms = query_terms("Привет", None);
        // The raw token survives; stems that coincide with it are deduplicated.
        assert_eq!(terms, vec!["привет".to_string()]);
        // A detected short Italian word stems once.
        let it = query_terms("traslocato", Some(Lang::It));
        assert_eq!(it, vec!["trasloc".to_string()]);
    }
}
