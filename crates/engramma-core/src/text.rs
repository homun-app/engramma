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
    supported_lang(info.lang())
}

/// The six bundled stemming languages, in the canonical order the unknown
/// fallback has always used.
pub const ALL_STEM_LANGS: [Lang; 6] = [Lang::It, Lang::En, Lang::Fr, Lang::De, Lang::Es, Lang::Pt];

fn supported_lang(l: WhatLang) -> Option<Lang> {
    match l {
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

/// Below this many significant words (stopwords out) a question is too short
/// for a trustworthy language detection (v0.5.3, P0): «Zebedeo» is a word of
/// no language in particular, and `whatlang` guessing German for it stemmed
/// the query away from the Italian index. The threshold is measured against
/// the recall bank of short questions (`tests/golden/queries-short.json`).
pub const MIN_CONFIDENT_QUERY_WORDS: usize = 4;

/// The stemming language of a QUESTION (v0.5.3, P0): `None` means
/// «uncertain — expand». A question is uncertain when it has fewer than
/// [`MIN_CONFIDENT_QUERY_WORDS`] significant words or when `whatlang` itself
/// calls its own detection unreliable; notes keep using [`detect_lang`],
/// which has a whole text to look at and a language that wins anyway (the
/// index was stemmed with it).
pub fn detect_query_lang(text: &str) -> Option<Lang> {
    let folded = strip_diacritics(&text.to_lowercase());
    let significant = tokenize(&folded)
        .into_iter()
        .filter(|t| !STOPWORDS.contains(t))
        .count();
    if significant < MIN_CONFIDENT_QUERY_WORDS {
        return None;
    }
    let info = whatlang::detect(text)?;
    if !info.is_reliable() {
        return None;
    }
    supported_lang(info.lang())
}

/// One significant word of a question: every form the index may hold for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryWord {
    /// The alternatives of this word, deduplicated: the raw token (which the
    /// index holds when the note had no language) and, on an uncertain
    /// question, one stem per language of the cell. They are ALTERNATIVES:
    /// the MATCH takes them as an OR.
    pub forms: Vec<String>,
    /// The raw form, when this word carries one (uncertain question): it is
    /// ALSO in `forms`, but it matches differently — see [`QueryWord::raw`].
    pub raw: Option<String>,
    /// The canonical form — the stem in the question's language, or in the
    /// cell's most frequent language when the question was uncertain (the
    /// raw token when the cell stems nothing). The adjacent-pair phrases and
    /// the whole-question phrase are built on these.
    pub primary: String,
}

impl QueryWord {
    /// The MATCH fragment of one form. A STEM is searched everywhere (the
    /// raw `title` column holds unstemmed text, and a title token that IS
    /// the stem is a fair match). The RAW form is searched in the STEMMED
    /// columns — `body_norm` (which holds the raw tokens exactly when the
    /// note had no language, the reason the raw form exists) and the tags —
    /// EXCEPT when it is the whole question: titles are always unstemmed
    /// and a raw word of a LONGER question would match every note titled
    /// with it, drowning the graph answers under title-weight hits (found
    /// on the golden semantic bank: «dove lavora la sorella di Giulia»).
    /// A ONE-WORD question, though, is a name lookup, and the title is
    /// where a name lives («Luca» must rank «Luca Ferrari» first: found on
    /// the short-question bank).
    #[must_use]
    pub fn fragment(form: &str, raw: bool, single_word_question: bool) -> String {
        let q = form.replace('"', "");
        if raw && !single_word_question {
            format!("{{body_norm tags_text}} : \"{q}\"")
        } else {
            format!("\"{q}\"")
        }
    }
}

/// The terms of a question, word by word (v0.5.3, P0).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueryTerms {
    /// One entry per significant query word, in order.
    pub words: Vec<QueryWord>,
}

impl QueryTerms {
    /// Whether the question left no usable word at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Every form with the MATCH fragment it would use — the exact needle
    /// the quarantined-document correction must repeat.
    #[must_use]
    pub fn fragments(&self) -> Vec<(String, String)> {
        let single = self.words.len() == 1;
        let mut out: Vec<(String, String)> = Vec::new();
        for w in &self.words {
            for f in &w.forms {
                let raw = w.raw.as_ref() == Some(f);
                if !out.iter().any(|(t, _)| t == f) {
                    out.push((f.clone(), QueryWord::fragment(f, raw, single)));
                }
            }
        }
        out
    }

    /// Every form of every word, deduplicated, in insertion order — the list
    /// the document frequencies and the ubiquitous-term dropping look at.
    #[must_use]
    pub fn flat(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for w in &self.words {
            for f in &w.forms {
                if !out.contains(f) {
                    out.push(f.clone());
                }
            }
        }
        out
    }
}

/// Italian and English question words and fillers: they match almost every
/// document and carry no signal, so they are dropped from queries (raw and
/// after stemming — "è" stems to "e" in Italian, both spellings must go).
const STOPWORDS: &[&str] = &[
    // italiano
    "dove", "chi", "che", "cosa", "quando", "quale", "quali", "quanto", "quanti", "quanta",
    "quante", "come", "perche", "quale", "e", "ed", "o", "ma", "non", "si", "no", "io", "tu",
    "lui", "lei", "noi", "voi", "loro", "il", "lo", "la", "i", "gli", "le", "un", "uno", "una",
    "di", "a", "al", "allo", "alla", "ai", "agli", "alle", "da", "dal", "dallo", "dalla", "dai",
    "dagli", "dalle", "in", "nel", "nello", "nella", "nei", "negli", "nelle", "con", "su", "sul",
    "sullo", "sulla", "sui", "sugli", "sulle", "per", "tra", "fra", "del", "dello", "della", "dei",
    "degli", "delle", "era", "erano", "ero", "sono", "stato", "stata", "stati", "state", "essere",
    "fa", "fanno", "fatto", "ha", "hai", "hanno", "ho", "sia", "va", "vanno", "questo", "questa",
    "questi", "queste", "quello", "quella", "quelli", "quelle", "cui", "ne", "ci", "vi", "mi",
    "ti", "se", "anche", "solo", "molto", "poco", "piu", "meno", "ogni", "tutti", "tutto", "tutta",
    "tutte", "alcuni", "alcuno", "nessuno", "altro", "altra", "altri", "altre", "senza", "verso",
    "cio", "adesso", "ancora", "prima", "dopo", "sempre", "mai", "poi", "gia", "qui", "la",
    // inglese
    "the", "a", "an", "of", "in", "on", "at", "to", "for", "with", "by", "is", "are", "was", "were",
    "be", "been", "who", "whats", "what", "where", "when", "why", "how", "does", "do", "did",
    "his", "her", "their", "its", "and", "or", "but", "not", "now", "still", "then",
];

/// The query terms for the FTS index, word by word: the forms the index may
/// hold for each significant query word. With an explicit language (or a
/// confident detection, [`detect_query_lang`]) each word keeps its single
/// stem, exactly as before; an UNCERTAIN question (short or unreliably
/// detected) expands every word into its raw form plus one stem per language
/// of the cell — the six bundled stemmers when the cell stems nothing.
/// Recall-oriented: BM25 still ranks better-fitting documents first.
///
/// `cell_langs` is the stemming-language histogram of the cell's visible
/// notes, most frequent first (the caller builds and caches it); it decides
/// the expansion set and, when the question is uncertain, the canonical form
/// of each word (the stem in the most frequent language — the adjacent-pair
/// phrases of v0.2.1 P1.7 are built on those).
pub fn query_terms(text: &str, lang: Option<Lang>, cell_langs: &[Lang]) -> QueryTerms {
    let lowered = text.to_lowercase();
    let tokens = tokenize(&lowered);
    let detected = lang.or_else(|| detect_query_lang(text));
    // the expansion set: empty when the language is confident
    let expand: &[Lang] = match detected {
        Some(_) => &[],
        None if cell_langs.is_empty() => &ALL_STEM_LANGS,
        None => cell_langs,
    };
    // the canonical language of the pairs: the confident one, else the most
    // frequent of the cell, else none (the cell stems nothing: the index
    // holds raw forms, and so does `primary`)
    let primary_lang = detected.or_else(|| cell_langs.first().copied());
    let mut words: Vec<QueryWord> = Vec::new();
    for token in tokens {
        // Stopwords are dropped both raw and after stemming ("è" stems to
        // "e" in Italian: both spellings must go).
        if STOPWORDS.contains(&token) {
            continue;
        }
        let mut forms: Vec<String> = Vec::new();
        let mut push = |f: String| {
            if !STOPWORDS.contains(&f.as_str()) && !forms.contains(&f) {
                forms.push(f);
            }
        };
        let mut raw: Option<String> = None;
        match detected {
            Some(l) => push(normalize(token, Some(l))),
            None => {
                // the raw form: what the index holds when the note had no
                // language of its own
                push(token.to_string());
                raw = Some(token.to_string());
                for l in expand {
                    push(normalize(token, Some(*l)));
                }
            }
        }
        if forms.is_empty() {
            continue;
        }
        let prim = match primary_lang {
            Some(l) => normalize(token, Some(l)),
            None => token.to_string(),
        };
        // the canonical form must be one of the word's forms: a stem the
        // stopword filter threw away falls back to the first alternative
        let primary = if forms.contains(&prim) {
            prim
        } else {
            forms[0].clone()
        };
        words.push(QueryWord {
            forms,
            raw,
            primary,
        });
    }
    QueryTerms { words }
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
                out.push_str(latin1(c));
            }
            // Latin Extended-A
            '\u{0100}'..='\u{017F}' => out.push_str(latin_ext_a(c)),
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

/// v0.5 (P1): the duplicate test of a proposal. Deliberately dumber than
/// [`normalize`]: no stemming (two facts that share every stem but differ in
/// one word are NOT duplicates), only the three normalisations the spec
/// names — case, whitespace runs, punctuation. Punctuation acts as a
/// separator ("nero." == "nero", "Roma,5" == "Roma 5"), so a decimal comma
/// and a space read the same: for a refusal test that over-merges is the
/// safe direction (it asks the human, it never silently stores a twin).
pub fn normalize_for_duplicate(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    let mut wrote_any = false;
    for c in text.chars() {
        if c.is_alphanumeric() {
            if pending_space && wrote_any {
                out.push(' ');
            }
            pending_space = false;
            wrote_any = true;
            for lc in c.to_lowercase() {
                out.push(lc);
            }
        } else {
            // whitespace AND punctuation: separators, collapsed
            pending_space = true;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    /// v0.5 (P1): the duplicate normalisation, decided and pinned here.
    #[test]
    fn duplicate_normalisation() {
        use super::normalize_for_duplicate as n;
        // case, whitespace runs, punctuation
        assert_eq!(n("Fabio lavora a Milano."), n("fabio lavora a milano"));
        assert_eq!(n("FABIO   LAVORA A MILANO!"), n("Fabio lavora a Milano."));
        assert_eq!(n("via Roma, 5"), n("via Roma 5"));
        assert_eq!(n("caffè"), n("CAFFÈ"), "accents are letters, not noise");
        // the trim: leading and trailing separators vanish
        assert_eq!(n("  Fabio  "), n("Fabio"));
        // and what must NOT merge: different facts stay different
        assert_ne!(n("Fabio lavora a Milano."), n("Fabio lavora a Torino."));
        assert_ne!(
            n("Fabio parla inglese"),
            n("Fabio parla inglese e francese")
        );
        // no stemming: two facts sharing every stem differ in one word
        assert_ne!(n("il gatto nero corre"), n("il gatto nero correva"));
    }

    use super::*;

    #[test]
    fn diacritics() {
        assert_eq!(
            strip_diacritics("città perù Übung déjà"),
            "citta peru ubung deja"
        );
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
        assert_eq!(
            detect_lang("The quick brown fox jumps over the lazy dog"),
            Some(Lang::En)
        );
        // Unsupported languages get no stemming.
        assert_eq!(detect_lang("Привет, как дела сегодня"), None);
    }

    #[test]
    fn query_terms_include_all_languages_when_unknown() {
        // A word in an unsupported language (Russian): raw token plus the
        // stems of every language of an empty cell (the six, deduplicated —
        // stems that coincide with the raw form collapse).
        let terms = query_terms("Привет", None, &[]);
        assert_eq!(terms.flat(), vec!["привет".to_string()]);
        // A confident short Italian question stems once, as ever.
        let it = query_terms("traslocato", Some(Lang::It), &[]);
        assert_eq!(it.flat(), vec!["trasloc".to_string()]);
    }

    /// v0.5.3 (P0): a one-word question is never a confident detection.
    #[test]
    fn short_questions_are_uncertain_whatever_the_guess() {
        for q in [
            "Zebedeo",
            "tonno",
            "Aurora",
            "martedì",
            "gatto Zebedeo",
            "crocchette tonno",
        ] {
            assert_eq!(
                detect_query_lang(q),
                None,
                "`{q}`: one to three words never carry a language"
            );
        }
        // a real sentence is confident
        assert_eq!(
            detect_query_lang("il gatto di prova si chiama Zebedeo e mangia crocchette"),
            Some(Lang::It)
        );
    }

    /// v0.5.3 (P0): an uncertain question expands over the CELL's languages.
    #[test]
    fn uncertain_questions_expand_over_the_cell_languages() {
        // an Italian-only cell: raw form plus the Italian stem — «Zebedeo»
        // finds the note whatever `whatlang` guesses for one word
        let terms = query_terms("Zebedeo", None, &[Lang::It]);
        assert_eq!(terms.words.len(), 1);
        assert_eq!(
            terms.words[0].forms,
            vec!["zebedeo".to_string(), normalize("zebedeo", Some(Lang::It))]
        );
        assert_eq!(terms.words[0].primary, normalize("zebedeo", Some(Lang::It)));
        // a cell that stems nothing: the six languages, as ever
        let six = query_terms("tonno", None, &[]);
        assert!(six.flat().contains(&"tonn".to_string()), "{six:?}");
        // a mixed cell expands only over what it holds: Italian and English
        // give two distinct forms of «tonno» (dedup collapses what coincides)
        let mixed = query_terms("tonno", None, &[Lang::It, Lang::En]);
        assert_eq!(
            mixed.words[0].forms,
            vec!["tonno".to_string(), "tonn".to_string()]
        );
    }

    /// v0.5.3 (P0): a question `whatlang` trusts stems exactly as an
    /// explicit language would — no expansion, one form per word, the
    /// canonical form IS that form.
    #[test]
    fn confident_questions_stem_as_they_always_did() {
        let q = "il gatto di prova si chiama Zebedeo e mangia crocchette";
        let detected = query_terms(q, None, &[Lang::It]);
        let explicit = query_terms(q, Some(Lang::It), &[]);
        assert_eq!(
            detected, explicit,
            "a reliable detection is the explicit language"
        );
        assert_eq!(detected.words.len(), 6, "{detected:?}");
        assert!(
            detected
                .words
                .iter()
                .all(|w| w.forms.len() == 1 && w.forms[0] == w.primary),
            "one form per word, and it is the canonical one: {detected:?}"
        );
    }
}
