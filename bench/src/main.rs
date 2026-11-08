// SPDX-License-Identifier: Apache-2.0

//! `engramma-bench`: deterministic synthetic memory (fixed seed), then
//! insert / search / open / recall measurements (§11). Run in `--release`.
//!
//! ```text
//! engramma-bench [--notes 10000] [--notes 100000] [--out PATH.md] [--runs 3]
//!                [--fake-embedder]
//! ```
//!
//! `--notes` is repeatable (bare numbers also work). Every size runs `--runs`
//! times (default 3) and the table reports the **median** of each metric,
//! for two corpus variants: **viva** (current facts carry a recent
//! `updated_at`, like a living memory — the recency weight matters) and
//! **uniform** (the same `updated_at` for every note — pure text+graph
//! ranking). Without `--out` the report goes to `bench/RESULTS.md`; with it,
//! only there (RESULTS.md is left alone).
//!
//! `--fake-embedder` replaces the whole run with the profiling probe
//! (`vprof`): a deterministic fake embedder, no model — it measures the
//! engine without the embedding cost. It times insert, one `embed_pending`
//! over every note, vector-only and hybrid search, and the raw int8 scan
//! (`FlatI8::search`) on synthetic vectors.

use std::path::PathBuf;
use std::time::Instant;

use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::search::{Fusion, Mode, Query, Semantic};
use engramma_core::{Cell, Confidence, EmbedError, Embedder, Status, TextKind};
use time::OffsetDateTime;

/// Fixed seed: same corpus on every run, on every machine.
const SEED: u64 = 0x2026_1105;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// A deterministic f32 in [-1, 1) from the top bits (as in the core's
    /// vector tests).
    fn next_f32(&mut self) -> f32 {
        ((self.next() >> 40) as f32 / 16_777_216.0) - 1.0
    }
}

const FIRST: [&str; 32] = [
    "Anna", "Bruno", "Carla", "Dario", "Elena", "Fabio", "Giulia", "Hugo", "Irene", "Jonas",
    "Katia", "Luca", "Marta", "Nadia", "Omar", "Paola", "Quirino", "Rosa", "Sara", "Tommaso",
    "Umberto", "Vera", "Walter", "Xenia", "Yuri", "Zita", "Aldo", "Bianca", "Cesare", "Delfina",
    "Ettore", "Fiora",
];
const LAST: [&str; 24] = [
    "Rossi",
    "Bianchi",
    "Ferrari",
    "Russo",
    "Brambilla",
    "Conti",
    "Esposito",
    "Ricci",
    "Marino",
    "Greco",
    "Bruno",
    "Gallo",
    "Contini",
    "Lombardi",
    "Moretti",
    "Fontana",
    "Barbieri",
    "Rinaldi",
    "Caruso",
    "Morelli",
    "Ferrara",
    "Gatti",
    "Martini",
    "Leone",
];
const CITIES: [&str; 18] = [
    "Roma", "Milano", "Torino", "Genova", "Bologna", "Firenze", "Venezia", "Verona", "Trieste",
    "Napoli", "Bari", "Palermo", "Cagliari", "Perugia", "Siena", "Padova", "Brescia", "Mantova",
];
const COMPANIES: [&str; 14] = [
    "Studio Aurora",
    "TechNova",
    "Ferro&Figli",
    "Archimedea",
    "VerdeAcqua",
    "Datariver",
    "Officine Lupini",
    "CasaModerna",
    "Numeria",
    "PonteDigital",
    "AlbaCoop",
    "SudProgetti",
    "NordEstIngegneria",
    "MetroStudio",
];
const PROJECTS: [&str; 12] = [
    "Progetto Aurora",
    "Ponte Nuovo",
    "Biblioteca Civica",
    "Campus Verde",
    "Museo del Porto",
    "Ospedale Est",
    "Quartiere Sole",
    "Stadio Nord",
    "Parco Fluviale",
    "Torre Ovest",
    "Campus Mare",
    "Rete Metropolitana",
];
const FOODS: [&str; 8] = [
    "la pasta alla norma",
    "il risotto allo zafferano",
    "la pizza margherita",
    "la ribollita",
    "gli gnocchi di patate",
    "il pesto alla genovese",
    "la caponata",
    "le lasagne al forno",
];

/// A generated question with its expected note.
struct Question {
    text: String,
    as_of: Option<OffsetDateTime>,
    /// Acceptable answer note ids (the fact, and where present the person
    /// note, whose body carries the same information).
    expect_ids: Vec<String>,
    /// Benchmark category: fattuali | parafrasi | multi-salto | inglese.
    category: &'static str,
}

struct Corpus {
    notes: Vec<Note>,
    questions: Vec<Question>,
}

fn ts(year: i32, month: u8, day: u8) -> At {
    let month: time::Month = month.try_into().unwrap_or(time::Month::January);
    let date = time::Date::from_calendar_date(year, month, day)
        .unwrap_or_else(|_| time::Date::from_calendar_date(2024, time::Month::January, 1).unwrap());
    At(date.midnight().assume_utc())
}

// A data-builder with every field of a note; the argument count is the point.
#[allow(clippy::too_many_arguments)]
fn base_note(
    id: ulid::Ulid,
    note_type: NoteType,
    title: String,
    body: String,
    tags: Vec<String>,
    valid_from: Option<engramma_core::Validity>,
    valid_to: Option<engramma_core::Validity>,
    supersedes: Vec<ulid::Ulid>,
    created: At,
) -> Note {
    Note {
        id,
        note_type,
        title: Some(title),
        body,
        tags,
        scope: None,
        lang: Some(Lang::It),
        source: "bench".into(),
        confidence: Confidence::Stated,
        status: Status::Active,
        valid_from,
        valid_to,
        supersedes,
        created_at: created,
        updated_at: created,
        extensions: vec![],
    }
}

fn validity(y: i64, m: u8, d: u8) -> engramma_core::Validity {
    engramma_core::Validity::parse(&format!("{y:04}-{m:02}-{d:02}"))
        .unwrap_or_else(|_| engramma_core::Validity::parse("2024-01-01").unwrap(/* fallback */))
}

/// Build the corpus: `target` notes of story around people, places, projects,
/// with changes over time, plus questions with expected answers. With
/// `uniform_updated_at` every note gets the same `updated_at`, so the
/// recency weight cannot tell facts from fillers.
fn build_corpus(target: usize, uniform_updated_at: bool) -> Corpus {
    let mut rng = Rng(SEED);
    let mut notes: Vec<Note> = Vec::with_capacity(target + 16);
    let mut questions: Vec<Question> = Vec::new();
    let mut counter: u64 = 0;
    let mut nid = || -> ulid::Ulid {
        counter += 1;
        // Fixed timestamp, increasing random part: sortable, deterministic.
        ulid::Ulid::from_parts(1_800_000_000_000, u128::from(counter))
    };

    let n_people = (target / 20).clamp(50, 3_000);
    // person index -> (name, city_current, company, partner_idx).
    // v0.2.1 (P1.7): partners are MUTUAL (a real vault writes the relation
    // on both pages). The one-sided v0.2 draw made "il partner di X" a
    // question whose answer-note never named X, so no text signal could
    // find it and the graph route drowned in coincidental surname matches.
    let mut people: Vec<(String, usize, usize, Option<usize>)> = Vec::new();
    for i in 0..n_people {
        // Unique names: combos cycle, then a generation number is appended,
        // so no namesake can blur a question's expected answer.
        let generation = i / (FIRST.len() * LAST.len());
        let name = format!(
            "{} {}{}",
            FIRST[i % FIRST.len()],
            LAST[(i / FIRST.len()) % LAST.len()],
            if generation == 0 {
                String::new()
            } else {
                format!(" {generation}")
            }
        );
        people.push((
            name,
            rng.below(CITIES.len() as u64) as usize,
            rng.below(COMPANIES.len() as u64) as usize,
            None,
        ));
    }
    for i in 0..n_people {
        if people[i].3.is_none() && rng.below(3) == 0 {
            // a mutual pair with a random unpaired person (not oneself)
            let mut j = rng.below(n_people as u64) as usize;
            let mut tries = 0;
            while (j == i || people[j].3.is_some()) && tries < 8 {
                j = rng.below(n_people as u64) as usize;
                tries += 1;
            }
            if j != i && people[j].3.is_none() {
                people[i].3 = Some(j);
                people[j].3 = Some(i);
            }
        }
    }

    // Person notes (with relations) and their histories.
    for (name, city, company, partner) in people.iter() {
        let person_id = nid();
        let mut body = format!("{name}. ");
        if let Some(p) = partner {
            body.push_str(&format!("Partner di [[{}]]. ", people[*p].0));
        }
        body.push_str(&format!(
            "\n\n## Relations\n- lives_in:: [[{}]] (from 2024-01-01)\n- works_at:: [[{}]] (from 2024-01-01)\n",
            CITIES[*city], COMPANIES[*company]
        ));
        if let Some(p) = partner {
            body.push_str(&format!("- partner_of:: [[{}]]\n", people[*p].0));
        }
        let mut person_note = base_note(
            person_id,
            NoteType::Person,
            name.clone(),
            body,
            vec!["persona".into()],
            None,
            None,
            vec![],
            ts(2024, 1, 1),
        );
        // v0.2.1 (P1.7): a person page is living memory — the vault's other
        // current facts carry a recent `updated_at` and the person note did
        // not, which the multi-hop diagnosis pinned as the reason the
        // partner's note (a text match on «partner», dated 2024) sank below
        // six-month-old fillers under the 180-day recency half-life.
        person_note.updated_at = ts(2026, 10, 1 + (rng.below(27) as u8));
        notes.push(person_note);

        // Residence history: 2020–2023 in another city, superseded.
        let old_city = rng.below(CITIES.len() as u64) as usize;
        let old_id = nid();
        notes.push(base_note(
            old_id,
            NoteType::Fact,
            format!("{name} abitava a {}", CITIES[old_city]),
            format!(
                "{name} abitava a [[{}]] dal 2020 al 2023.",
                CITIES[old_city]
            ),
            vec![format!("casa/{}", slug(CITIES[old_city]))],
            Some(validity(2020, 2, 1)),
            Some(validity(2023, 12, 31)),
            vec![],
            ts(2024, 1, 2),
        ));
        let cur_id = nid();
        let mut cur = base_note(
            cur_id,
            NoteType::Fact,
            format!("{name} abita a {}", CITIES[*city]),
            format!("{name} abita a [[{}]] dal gennaio 2024.", CITIES[*city]),
            vec![format!("casa/{}", slug(CITIES[*city]))],
            Some(validity(2024, 1, 1)),
            None,
            vec![old_id],
            ts(2024, 1, 3),
        );
        cur.updated_at = ts(2026, 10, 1 + (rng.below(27) as u8));
        notes.push(cur);
        // Old note becomes superseded in the corpus snapshot.
        if let Some(old) = notes.iter_mut().find(|n| n.id == old_id) {
            old.status = Status::Superseded;
        }

        // Work history.
        let old_co = rng.below(COMPANIES.len() as u64) as usize;
        let old_work = nid();
        notes.push(base_note(
            old_work,
            NoteType::Fact,
            format!("{name} lavorava da {}", COMPANIES[old_co]),
            format!(
                "{name} lavorava presso [[{}]] fino al 2023.",
                COMPANIES[old_co]
            ),
            vec![format!("lavoro/{}", slug(COMPANIES[old_co]))],
            Some(validity(2019, 3, 1)),
            Some(validity(2023, 12, 31)),
            vec![],
            ts(2024, 1, 4),
        ));
        let cur_work = nid();
        let mut work_note = base_note(
            cur_work,
            NoteType::Fact,
            format!("{name} lavora da {}", COMPANIES[*company]),
            format!(
                "{name} lavora presso [[{}]] come dipendente dal 2024.",
                COMPANIES[*company]
            ),
            vec![format!("lavoro/{}", slug(COMPANIES[*company]))],
            Some(validity(2024, 1, 1)),
            None,
            vec![old_work],
            ts(2024, 1, 5),
        );
        work_note.updated_at = ts(2026, 10, 1 + (rng.below(27) as u8));
        notes.push(work_note);
        if let Some(old) = notes.iter_mut().find(|n| n.id == old_work) {
            old.status = Status::Superseded;
        }

        // A food preference that changes.
        if rng.below(2) == 0 {
            let food_old = FOODS[rng.below(FOODS.len() as u64) as usize];
            let food_new = FOODS[rng.below(FOODS.len() as u64) as usize];
            let old_p = nid();
            notes.push(base_note(
                old_p,
                NoteType::Preference,
                format!("{name} e il cibo"),
                format!("{name} prediligeva {food_old} fino al 2024."),
                vec!["cibo".into()],
                Some(validity(2021, 5, 1)),
                Some(validity(2023, 12, 31)),
                vec![],
                ts(2024, 1, 6),
            ));
            let cur_p = nid();
            let mut pref_note = base_note(
                cur_p,
                NoteType::Preference,
                format!("Il piatto preferito di {name}"),
                format!("Dal 2024 a {name} piace soprattutto {food_new}."),
                vec!["cibo".into()],
                Some(validity(2024, 1, 1)),
                None,
                vec![old_p],
                ts(2024, 1, 7),
            );
            pref_note.updated_at = ts(2026, 10, 1 + (rng.below(27) as u8));
            notes.push(pref_note);
            if let Some(old) = notes.iter_mut().find(|n| n.id == old_p) {
                old.status = Status::Superseded;
            }
            if rng.below(4) == 0 {
                questions.push(Question {
                    text: format!("che cosa mangia volentieri {name}"),
                    as_of: None,
                    expect_ids: vec![cur_p.to_string()],
                    category: "fattuali",
                });
            }
        }

        // Projects and filler facts around this person.
        if rng.below(3) == 0 {
            let proj = PROJECTS[rng.below(PROJECTS.len() as u64) as usize];
            let pid = nid();
            notes.push(base_note(
                pid,
                NoteType::Project,
                proj.to_string(),
                format!("{proj}: progetto seguito da [[{name}]] con scadenza dicembre 2026."),
                vec!["lavoro/progetti".into()],
                None,
                None,
                vec![],
                ts(2024, 6, 1),
            ));
        }

        // A couple of questions per person (sampled).
        match rng.below(6) {
            0 => {
                questions.push(Question {
                    text: format!("dove abita {name}"),
                    as_of: None,
                    expect_ids: vec![cur_id.to_string(), person_id.to_string()],
                    category: "fattuali",
                });
                // paraphrase at a fixed model: no shared root with the note
                questions.push(Question {
                    text: format!("in quale citta risiede {name}"),
                    as_of: None,
                    expect_ids: vec![cur_id.to_string(), person_id.to_string()],
                    category: "parafrasi",
                });
                questions.push(Question {
                    text: format!("Where does {} live these days?", name),
                    as_of: None,
                    expect_ids: vec![cur_id.to_string(), person_id.to_string()],
                    category: "inglese",
                });
            }
            1 => {
                questions.push(Question {
                    text: format!("dove lavora {name}"),
                    as_of: None,
                    expect_ids: vec![cur_work.to_string(), person_id.to_string()],
                    category: "fattuali",
                });
                questions.push(Question {
                    text: format!("in quale impiego e' impegnato {name}"),
                    as_of: None,
                    expect_ids: vec![cur_work.to_string(), person_id.to_string()],
                    category: "parafrasi",
                });
                questions.push(Question {
                    text: format!("Where does {} work?", name),
                    as_of: None,
                    expect_ids: vec![cur_work.to_string(), person_id.to_string()],
                    category: "inglese",
                });
            }
            2 => {
                questions.push(Question {
                    text: format!("dove abitava {name}"),
                    as_of: Some(validity(2022, 6, 15).start()),
                    expect_ids: vec![old_id.to_string()],
                    category: "fattuali",
                });
                questions.push(Question {
                    text: format!("qual era la residenza precedente di {name}"),
                    as_of: Some(validity(2022, 6, 15).start()),
                    expect_ids: vec![old_id.to_string()],
                    category: "parafrasi",
                });
            }
            _ => {}
        }
    }

    // Filler facts to reach the target size.
    let fillers = target.saturating_sub(notes.len());
    for _ in 0..fillers {
        let i = rng.below(n_people as u64) as usize;
        let (ref name, ref city, ref company, _) = people[i];
        let months = [
            "gennaio",
            "febbraio",
            "marzo",
            "aprile",
            "maggio",
            "giugno",
            "luglio",
            "agosto",
            "settembre",
            "ottobre",
            "novembre",
            "dicembre",
        ];
        let m = months[rng.below(12) as usize];
        let body = match rng.below(4) {
            0 => format!(
                "[[{name}]] ha visitato [[{}]] a {m} 2025 per una breve vacanza. #viaggio",
                CITIES[*city]
            ),
            1 => format!(
                "[[{name}]] ha partecipato alla riunione di [[{}]] a {m} 2025 sull'ufficio nuovo.",
                COMPANIES[*company]
            ),
            2 => format!(
                "Appunto su [[{name}]]: a {m} 2025 rinnovo dell'abbonamento e visita da [[{}]].",
                CITIES[*city]
            ),
            _ => format!(
                "[[{name}]] e il progetto: a {m} 2025 revisione dei documenti con [[{}]].",
                COMPANIES[*company]
            ),
        };
        let id = nid();
        // v0.2.1 (P1.7): the filler title keeps the month, the NAME stays in
        // the body — with the name in the title, twenty-odd fillers per
        // person crowded the BM25 window on every name query and pushed the
        // notes that actually answer relationship questions out of the
        // candidates.
        notes.push(base_note(
            id,
            NoteType::Fact,
            format!("Appunto di {m}"),
            body,
            vec![format!("casa/{}", slug(CITIES[*city]))],
            Some(validity(
                2025,
                1 + rng.below(12) as u8,
                1 + rng.below(27) as u8,
            )),
            None,
            vec![],
            ts(2025, 1 + rng.below(12) as u8, 2),
        ));
    }

    // Multi-hop questions: "dove lavora il partner di X" — the answer is the
    // partner's current-work note (found by its deterministic title).
    for (i, (name, _, _, partner)) in people.iter().enumerate() {
        if i % 7 == 0
            && let Some(p) = partner
        {
            let pname = &people[*p].0;
            let work = notes.iter().find(|n| {
                n.note_type == NoteType::Fact
                    && n.title.as_deref()
                        == Some(format!("{pname} lavora da {}", COMPANIES[people[*p].2]).as_str())
            });
            let partner_note = notes.iter().find(|n| {
                n.note_type == NoteType::Person && n.title.as_deref() == Some(pname.as_str())
            });
            if let Some(w) = work {
                let mut ids = vec![w.id.to_string()];
                if let Some(pn) = partner_note {
                    ids.push(pn.id.to_string());
                }
                questions.push(Question {
                    text: format!("dove lavora il partner di {name}"),
                    as_of: None,
                    expect_ids: ids,
                    category: "multi-salto",
                });
            }
        }
    }

    if uniform_updated_at {
        let fixed = ts(2026, 10, 28);
        for n in notes.iter_mut() {
            n.updated_at = fixed;
        }
    }
    Corpus { notes, questions }
}

fn slug(s: &str) -> String {
    s.to_lowercase()
        .replace([' ', '&', '-'], "_")
        .replace("à", "a")
        .replace("è", "e")
        .replace('ò', "o")
}

// ---------------------------------------------------------------------------
// profiling probe (--fake-embedder): the engine without the model
// ---------------------------------------------------------------------------

/// The probe's deterministic fake embedder (from `vprof`): an FNV-1a seed
/// off the text drives a xorshift over the dimensions, then L2. No model is
/// loaded: what it measures is the vector machinery (storage, index, fusion)
/// and the text pipeline, never the embedding cost.
struct FakeEmbedder {
    dim: usize,
}

impl Embedder for FakeEmbedder {
    fn model_id(&self) -> &str {
        "fake@1"
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn max_tokens(&self) -> usize {
        512
    }
    fn embed(&self, texts: &[&str], _k: TextKind) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| {
                let mut s: u64 = t.bytes().fold(1469598103934665603_u64, |h, b| {
                    (h ^ u64::from(b)).wrapping_mul(1099511628211)
                }) | 1;
                let mut v: Vec<f32> = (0..self.dim)
                    .map(|_| {
                        s ^= s << 13;
                        s ^= s >> 7;
                        s ^= s << 17;
                        (s % 2000) as f32 / 1000.0 - 1.0
                    })
                    .collect();
                let n = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                v.iter_mut().for_each(|x| *x /= n);
                v
            })
            .collect())
    }
}

/// One probe measurement over a fresh cell.
struct FakeReport {
    notes: usize,
    insert_s: f64,
    embed_s: f64,
    vector_p50_ms: f64,
    vector_p95_ms: f64,
    hybrid_p50_ms: f64,
    hybrid_p95_ms: f64,
    /// The raw int8 scan on synthetic vectors (`FlatI8::search`, m = 200).
    top_int_p50_ms: f64,
    /// Index build from stored rows (the first search), in ms.
    index_build_ms: f64,
}

/// Insert `n` probe notes in a fresh cell, embed them all with one
/// `embed_pending` call, then measure vector-only and hybrid search. The
/// corpus is the probe's own (fixed template, deterministic ULIDs), so the
/// numbers are comparable across runs and phases.
fn run_fake(n: usize, loops: usize, dir: &std::path::Path) -> FakeReport {
    let stem = format!("fake-{n}");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.join(format!("{stem}.sqlite{suffix}")));
    }
    let db = dir.join(format!("{stem}.sqlite"));
    let cell = Cell::create(&db, "bench", "bench").expect(/* fresh file */ "create cell");
    let t = At(OffsetDateTime::from_unix_timestamp(1_800_000_000).expect(/* valid */ "ts"));
    let t0 = Instant::now();
    for i in 0..n {
        let note = Note {
            id: ulid::Ulid::from_parts(1_800_000_000_000, u128::from(i as u64) + 1),
            note_type: NoteType::Fact,
            title: Some(format!("nota {i}")),
            body: format!(
                "Persona{} lavora a Citta{} sul progetto {} con [[Persona{}]].",
                i % 997,
                i % 101,
                i % 313,
                (i * 7) % 997
            ),
            tags: vec![],
            scope: None,
            lang: Some(Lang::It),
            source: "user".into(),
            confidence: Confidence::Stated,
            status: Status::Active,
            valid_from: None,
            valid_to: None,
            supersedes: vec![],
            created_at: t,
            updated_at: t,
            extensions: vec![],
        };
        cell.put_note(&note, "bench")
            .expect(/* generated notes are valid */ "put");
    }
    let insert_s = t0.elapsed().as_secs_f64();

    cell.add_model("fake@1", 384)
        .expect(/* bench embedder is valid */ "add model");
    let emb: std::sync::Arc<dyn Embedder> = std::sync::Arc::new(FakeEmbedder { dim: 384 });
    let t0 = Instant::now();
    let report = cell
        .embed_pending(emb.as_ref(), n + 1)
        .expect(/* valid notes */ "embed");
    let embed_s = t0.elapsed().as_secs_f64();
    assert_eq!(report.remaining, 0, "the probe embeds everything");

    let now = OffsetDateTime::from_unix_timestamp(1_810_000_000).expect(/* fixed */ "ts");
    let latencies = |mode: Mode, index_build: &mut f64| -> (f64, f64) {
        let sem = Semantic::new(std::sync::Arc::clone(&emb)).with_mode(mode);
        let t0 = Instant::now();
        let _ = cell
            .search_semantic_with_now(&Query::text("warmup Persona12 Citta3").k(5), now, &sem)
            .expect(/* valid */ "search");
        *index_build = t0.elapsed().as_secs_f64() * 1000.0;
        let mut ts: Vec<f64> = Vec::with_capacity(loops);
        for i in 0..loops {
            let q = Query::text(format!(
                "dove lavora Persona{} progetto {}",
                i * 13 % 997,
                i % 313
            ))
            .k(5);
            let t = Instant::now();
            let _ = cell
                .search_semantic_with_now(&q, now, &sem)
                .expect(/* valid */ "search");
            ts.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        (percentile(ts.clone(), 0.5), percentile(ts, 0.95))
    };
    let mut build = 0.0_f64;
    let (vector_p50, vector_p95) = latencies(Mode::Vector, &mut build);
    let index_build_ms = build;
    let (hybrid_p50, hybrid_p95) = latencies(Mode::Hybrid, &mut 0.0_f64);

    let report = FakeReport {
        notes: n,
        insert_s,
        embed_s,
        vector_p50_ms: vector_p50,
        vector_p95_ms: vector_p95,
        hybrid_p50_ms: hybrid_p50,
        hybrid_p95_ms: hybrid_p95,
        top_int_p50_ms: fake_top_int_ms(n.max(1000), 384, loops),
        index_build_ms,
    };
    // the probe deletes its own cell: no 885 MB left in the temp folder
    drop(cell);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.join(format!("{stem}.sqlite{suffix}")));
    }
    report
}

/// The int8 flat scan in isolation: `n` synthetic unit vectors of `dim`
/// dimensions, one deterministic query set, `FlatI8::search` with m = 200.
fn fake_top_int_ms(n: usize, dim: usize, loops: usize) -> f64 {
    let mut rng = Rng(0x5EED_0F0F_1234_5678);
    let rows: Vec<engramma_core::IndexRow> = (0..n)
        .map(|i| {
            let mut v: Vec<f32> = (0..dim).map(|_| rng.next_f32()).collect();
            engramma_core::embed::l2_normalize(&mut v);
            engramma_core::IndexRow {
                note_id: format!("n{i:06}"),
                chunk: 0,
                vec: v,
            }
        })
        .collect();
    let mut index = engramma_core::FlatI8::new(dim);
    use engramma_core::VectorIndex as _;
    index.rebuild(rows);
    let mut ts: Vec<f64> = Vec::with_capacity(loops);
    for _ in 0..loops {
        let mut q: Vec<f32> = (0..dim).map(|_| rng.next_f32()).collect();
        engramma_core::embed::l2_normalize(&mut q);
        let t = Instant::now();
        let hits = index.search(&q, 200);
        ts.push(t.elapsed().as_secs_f64() * 1000.0);
        let _ = hits.len();
    }
    percentile(ts, 0.5)
}

/// Where the expected note of a multi-hop question ends up (P1.7): the
/// position in a wide result list, its score, the terms it matched and the
/// graph path that led there — and the same numbers for whatever beat it.
/// Run with `--multihop-debug [--notes N]`; five questions, text pipeline.
fn multihop_debug(target: usize, how_many: usize, dir: &std::path::Path) {
    let corpus = build_corpus(target, false);
    let db = dir.join(format!("mh-{target}.sqlite"));
    let _ = std::fs::remove_file(&db);
    let cell = Cell::create(&db, "bench", "bench").expect(/* fresh */ "create cell");
    for note in &corpus.notes {
        cell.put_note(note, "bench").expect(/* valid */ "put");
    }
    // v0.3: the multi-hop battle is in hybrid (the text branch alone finds the
    // partner note); embed with the 97M when present and diagnose both modes.
    let embedder = load_model("granite-embedding-97m-multilingual-r2");
    let sem = embedder.as_ref().map(|e| {
        cell.add_model(e.model_id(), e.dim())
            .expect(/* bench embedder is valid */ "add model");
        loop {
            let r = cell
                .embed_pending(e.as_ref(), 256)
                .expect(/* valid notes */ "embed");
            if r.done == 0 || r.remaining == 0 {
                break;
            }
        }
        Semantic::new(std::sync::Arc::clone(e))
    });
    let now = OffsetDateTime::from_unix_timestamp(1_810_000_000).expect(/* fixed */ "ts");
    // document frequencies of the corpus's most query-visible stems
    let dfs = cell
        .debug_column(
            "SELECT term || '=' || doc FROM notes_fts_vocab WHERE term IN \
             ('lavor','lavorav','abit','abitav','partner','citt','resid','mang','viv') \
             ORDER BY doc DESC",
        )
        .unwrap_or_default();
    let total = cell
        .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM notes WHERE status != 'deleted'")
        .unwrap_or_default();
    println!("df total={total:?} {dfs:?}");
    for (qi, q) in corpus
        .questions
        .iter()
        .filter(|q| q.category == "multi-salto")
        .take(how_many)
        .enumerate()
    {
        let mut query = Query::text(&q.text).k(20).graph_hops(2);
        query.lang = Some(Lang::It);
        if let Some(a) = q.as_of {
            query.filters.as_of = Some(a);
        }
        let text_hits = cell
            .search_with_now(&query, now)
            .expect(/* valid */ "search");
        println!("\n[{}] `{}` attese {:?}", qi + 1, q.text, {
            let mut v: Vec<String> = q
                .expect_ids
                .iter()
                .map(|id| id[id.len().saturating_sub(8)..].to_string())
                .collect();
            v.sort();
            v
        });
        for (label, hits) in [("testo", &text_hits)] {
            let expected_rank = hits
                .iter()
                .position(|h| q.expect_ids.contains(&h.note.id.to_string()));
            match expected_rank {
                Some(r) => {
                    let h = &hits[r];
                    println!(
                        "  [{label}] attesa al rango {} di {}: punteggio {:.4}, liste {:?}, via {:?}",
                        r + 1,
                        hits.len(),
                        h.raw_score,
                        h.explain.lists,
                        h.explain.via
                    );
                }
                None => println!("  [{label}] attesa ASSENTE dai primi {}", hits.len()),
            }
            for (r, h) in hits.iter().enumerate().take(6) {
                println!(
                    "  {}. {:.4} {} [{}] {:?}{}",
                    r + 1,
                    h.raw_score,
                    h.note.display_title(),
                    h.explain.lists.to_vec().join("+"),
                    h.explain.matched_terms,
                    h.explain
                        .via
                        .as_deref()
                        .map(|v| format!(" via {v}"))
                        .unwrap_or_default()
                );
            }
        }
        if let Some(sem) = &sem {
            let outcome = cell
                .search_semantic_with_now(&query, now, sem)
                .expect(/* valid */ "search");
            let hits = &outcome.hits;
            let expected_rank = hits
                .iter()
                .position(|h| q.expect_ids.contains(&h.note.id.to_string()));
            match expected_rank {
                Some(r) => {
                    let h = &hits[r];
                    println!(
                        "  [ibrido] attesa al rango {} di {}: punteggio {:.4}, liste {:?}, via {:?}",
                        r + 1,
                        hits.len(),
                        h.raw_score,
                        h.explain.lists,
                        h.explain.via
                    );
                }
                None => println!("  [ibrido] attesa ASSENTE dai primi {}", hits.len()),
            }
            for (r, h) in hits.iter().enumerate().take(8) {
                println!(
                    "  {}. {:.4} {} [{}] cos {:?}{}",
                    r + 1,
                    h.raw_score,
                    h.note.display_title(),
                    h.explain.lists.to_vec().join("+"),
                    h.explain.cosine.map(|c| format!("{c:.3}")),
                    h.explain
                        .via
                        .as_deref()
                        .map(|v| format!(" via {v}"))
                        .unwrap_or_default()
                );
            }
        }
    }
    let _ = std::fs::remove_file(&db);
}

/// Median of every probe metric across runs.
fn median_fake(runs: &[FakeReport]) -> FakeReport {
    let pick = |f: fn(&FakeReport) -> f64| median(runs.iter().map(f).collect());
    FakeReport {
        notes: runs[0].notes,
        insert_s: pick(|r| r.insert_s),
        embed_s: pick(|r| r.embed_s),
        vector_p50_ms: pick(|r| r.vector_p50_ms),
        vector_p95_ms: pick(|r| r.vector_p95_ms),
        hybrid_p50_ms: pick(|r| r.hybrid_p50_ms),
        hybrid_p95_ms: pick(|r| r.hybrid_p95_ms),
        top_int_p50_ms: pick(|r| r.top_int_p50_ms),
        index_build_ms: pick(|r| r.index_build_ms),
    }
}

fn fake_markdown(reports: &[FakeReport]) -> String {
    let mut out = String::from(
        "# Sonda engramma (embedder finto, `--fake-embedder`)\n\n\
         Il motore senza il modello: inserimento, un `embed_pending` su tutte le\
         note, ricerca solo-vettori e ibrida, e la scansione int8 isolata\
         (`FlatI8::search`, m=200, vettori sintetici). Mediana dei giri.\n\n\
         | note | inserimento (s) | embed_pending (s) | vettori p50 (ms) | vettori p95 (ms) | ibrido p50 (ms) | ibrido p95 (ms) | int8 p50 (ms) | build indice (ms) |\n\
         |---|---|---|---|---|---|---|---|---|\n",
    );
    for r in reports {
        out.push_str(&format!(
            "| {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.2} | {:.0} |\n",
            r.notes,
            r.insert_s,
            r.embed_s,
            r.vector_p50_ms,
            r.vector_p95_ms,
            r.hybrid_p50_ms,
            r.hybrid_p95_ms,
            r.top_int_p50_ms,
            r.index_build_ms,
        ));
    }
    if reports.len() >= 2 {
        let (a, b) = (&reports[0], &reports[reports.len() - 1]);
        out.push_str(&format!(
            "\nRapporto embed_pending {}/{}: **{:.1}×** (lineare ≈ {:.0}×; la soglia\
             della fase è 15).\n",
            b.notes,
            a.notes,
            b.embed_s / a.embed_s.max(1e-9),
            b.notes as f64 / a.notes as f64,
        ));
    }
    out
}

fn percentile(mut xs: Vec<f64>, p: f64) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((xs.len() as f64 - 1.0) * p).round() as usize;
    xs[idx.min(xs.len() - 1)]
}

struct RunReport {
    variant: &'static str,
    notes: usize,
    insert_notes_per_s: f64,
    open_ms: f64,
    search_p50_ms: f64,
    search_p95_ms: f64,
    text_only_p50_ms: f64,
    /// text+graph (hops 2) latencies: measured for every variant (P2.11)
    text_hops_p50_ms: f64,
    /// p95 of the same.
    text_hops_p95_ms: f64,
    recall_at_5: f64,
    mrr: f64,
    questions: usize,
    file_bytes: u64,
    /// v0.3.2 (P0.1): the compaction bill is the whole footprint (cell +
    /// WAL + snapshot files), before and after, in BOTH modes.
    footprint_before: u64,
    /// After the `--keep-snapshot` compaction (cell + logical snapshot).
    footprint_after_keep: u64,
    /// After the default checkpoint compaction (no second copy).
    footprint_after_plain: u64,
    /// The two compactions' wall times, in seconds.
    compact_keep_s: f64,
    compact_plain_s: f64,
    /// One `purge` at scale, in ms (v0.3.1): the whole forgetting path —
    /// FTS optimize, VACUUM, WAL checkpoint, snapshot scrub.
    purge_ms: f64,
    compacted_ops: i64,
    // v0.2 semantic metrics (None when no embedder ran)
    semantic: Option<SemanticReport>,
}

/// The v0.2 columns: embedding throughput, index build and memory, vector
/// and hybrid latencies, per-mode quality.
#[derive(Default, Clone)]
struct SemanticReport {
    model: String,
    embed_notes_per_s: f64,
    index_build_ms: f64,
    index_mem_mb: f64,
    graph_build_ms: f64,
    graph_nodes: usize,
    graph_edges: usize,
    graph_mem_mb: f64,
    graph_pushed_edges: f64,
    text_hops_p50_ms: f64,
    text_hops_p95_ms: f64,
    vector_p50_ms: f64,
    vector_p95_ms: f64,
    hybrid_warm_p50_ms: f64,
    hybrid_warm_p95_ms: f64,
    hybrid_cold_p50_ms: f64,
    hybrid_cold_p95_ms: f64,
    /// (mode, category) -> (recall@5, MRR, questions)
    quality: Vec<(String, String, f64, f64, usize)>,
}

#[allow(clippy::too_many_lines)]
fn run(target: usize, dir: &std::path::Path, uniform_updated_at: bool) -> RunReport {
    run_semantic(target, dir, uniform_updated_at, None)
}

fn run_semantic(
    target: usize,
    dir: &std::path::Path,
    uniform_updated_at: bool,
    embedder: Option<&std::sync::Arc<dyn Embedder>>,
) -> RunReport {
    let corpus = build_corpus(target, uniform_updated_at);
    let db = dir.join(format!("bench-{target}.sqlite"));
    let _ = std::fs::remove_file(&db);
    let cell = Cell::create(&db, "bench", "bench").expect(/* fresh file */ "create cell");

    // insert
    let t0 = Instant::now();
    for note in &corpus.notes {
        cell.put_note(note, "bench")
            .expect(/* generated notes are valid */ "put");
    }
    let insert_s = t0.elapsed().as_secs_f64();

    // open (fresh handle)
    let mut open_times: Vec<f64> = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let c = Cell::open(&db).expect(/* just created */ "open");
        let _ = c.meta();
        open_times.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let open_ms = open_times.iter().sum::<f64>() / open_times.len() as f64;

    // the graph index, measured before any search builds it (v0.3): the
    // plain runs print it, the semantic ones carry it in the report
    let (graph_build_ms, graph_stats) =
        cell.build_graph().expect(/* bench cell is open */ "graph");
    let (graph_nodes, graph_edges, graph_bytes) = graph_stats;
    eprintln!(
        "grafo: {graph_build_ms:.0} ms, {graph_nodes} nodi, {graph_edges} archi, {:.1} MB",
        graph_bytes as f64 / 1e6
    );

    // search — full pipeline, text-only (graph_hops = 0) and text+graph
    // (hops = 2, the §11 "ricerca testo+grafo" row: the PPR replaced the
    // per-level SQL walk)
    let now =
        OffsetDateTime::from_unix_timestamp(1_810_000_000).expect(/* fixed valid ts */ "ts");
    let mut text_only: Vec<f64> = Vec::new();
    let mut text_hops: Vec<f64> = Vec::new();
    for q in &corpus.questions {
        if q.expect_ids.is_empty() {
            continue;
        }
        let mut query = Query::text(&q.text).k(5).graph_hops(0);
        query.lang = Some(Lang::It);
        if let Some(a) = q.as_of {
            query.filters.as_of = Some(a);
        }
        let t = Instant::now();
        let _ = cell.search_with_now(&query, now);
        text_only.push(t.elapsed().as_secs_f64() * 1000.0);
        let mut query = Query::text(&q.text).k(5).graph_hops(2);
        query.lang = Some(Lang::It);
        if let Some(a) = q.as_of {
            query.filters.as_of = Some(a);
        }
        let t = Instant::now();
        let _ = cell.search_with_now(&query, now);
        text_hops.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let text_hops_p50_ms = percentile(text_hops.clone(), 0.5);
    let text_hops_p95_ms = percentile(text_hops.clone(), 0.95);
    let mut latencies: Vec<f64> = Vec::new();
    let mut hits_at_5 = 0_usize;
    let mut mrr_sum = 0.0;
    let mut answered = 0_usize;
    for q in &corpus.questions {
        if q.expect_ids.is_empty() {
            continue;
        }
        let mut query = Query::text(&q.text).k(5).graph_hops(2);
        query.lang = Some(Lang::It);
        if let Some(a) = q.as_of {
            query.filters.as_of = Some(a);
        }
        let t = Instant::now();
        let results = cell
            .search_with_now(&query, now)
            .expect(/* valid query */ "search");
        latencies.push(t.elapsed().as_secs_f64() * 1000.0);
        let ids: Vec<String> = results.iter().map(|h| h.note.id.to_string()).collect();

        // recall@5 and reciprocal rank
        if let Some(rank) = ids.iter().position(|i| q.expect_ids.contains(i)) {
            hits_at_5 += 1;
            mrr_sum += 1.0 / (rank + 1) as f64;
        }
        answered += 1;
    }

    // ------------------------------------------------------------------
    // v0.2: the semantic pass (skipped without an embedder)
    // ------------------------------------------------------------------
    let semantic = embedder.map(|embedder| {
        let model = embedder.model_id().to_string();
        cell.add_model(&model, embedder.dim())
            .expect(/* bench embedder is valid */ "add model");
        // v0.3: true-warm — the LRU holds every question of the corpus, the
        // "warm" passes re-embed nothing (the v0.2.1 256-slot cache did)
        cell.set_query_cache_capacity(corpus.questions.len().max(1))
            .expect(/* bench cell is open */ "lru");
        // the graph index: the FIRST build (measured at the top of this run)
        // is the number — calling build_graph again here would time a warm
        // no-op and the GraphIndex column would print 0 (v0.3.1, P2.11)
        let (graph_nodes, graph_edges, graph_bytes) = graph_stats;
        // embedding throughput: batched, threads (the embedder decides)
        let t0 = Instant::now();
        let mut total_done = 0_usize;
        loop {
            let r = cell
                .embed_pending(embedder.as_ref(), 256)
                .expect(/* valid notes */ "embed");
            total_done += r.done;
            if r.done == 0 || r.remaining == 0 {
                break;
            }
        }
        let embed_s = t0.elapsed().as_secs_f64();
        let embed_notes_per_s = total_done as f64 / embed_s.max(1e-9);

        // index build: the first semantic search builds it (lazy by design)
        let mut first = Query::text("Milano").k(5);
        first.lang = Some(Lang::It);
        let sem = Semantic::new(std::sync::Arc::clone(embedder));
        let t_build = Instant::now();
        let _ = cell
            .search_semantic(&first, &sem)
            .expect(/* valid */ "search");
        let index_build_ms = t_build.elapsed().as_secs_f64() * 1000.0;
        // index memory: int8 codes + f32 scales + row bookkeeping (~40 B/row)
        let vecs: usize = cell
            .debug_column("SELECT CAST(COUNT(*) AS TEXT) FROM note_vectors")
            .map(|v| v.first().and_then(|s| s.parse::<usize>().ok()).unwrap_or(0))
            .unwrap_or(0);
        let index_mem_mb = (vecs * (embedder.dim() + 4 + 40)) as f64 / 1e6;

        let now = OffsetDateTime::from_unix_timestamp(1_810_000_000).expect(/* fixed */ "ts");
        let mk_query = |q: &Question| {
            let mut query = Query::text(&q.text).k(5).graph_hops(2);
            query.lang = Some(Lang::It);
            if let Some(a) = q.as_of {
                query.filters.as_of = Some(a);
            }
            query
        };
        // cold pass: query embedding included (LRU empty)
        let mut cold: Vec<f64> = Vec::new();
        for q in &corpus.questions {
            if q.expect_ids.is_empty() {
                continue;
            }
            let t = Instant::now();
            let _ = cell.search_semantic_with_now(&mk_query(q), now, &sem);
            cold.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        // warm pass: same queries again — pure search, cache hits
        let mut warm: Vec<f64> = Vec::new();
        let mut pushed: Vec<usize> = Vec::new();
        for q in &corpus.questions {
            if q.expect_ids.is_empty() {
                continue;
            }
            let t = Instant::now();
            let outcome = cell.search_semantic_with_now(&mk_query(q), now, &sem);
            warm.push(t.elapsed().as_secs_f64() * 1000.0);
            pushed.push(outcome.map(|o| o.graph_pushed_edges).unwrap_or(0));
        }
        let graph_pushed_edges = pushed.iter().sum::<usize>() as f64 / pushed.len().max(1) as f64;
        // pure vector mode latencies (warm)
        let sem_vec = sem.clone().with_mode(Mode::Vector);
        let mut vlat: Vec<f64> = Vec::new();
        for q in &corpus.questions {
            if q.expect_ids.is_empty() {
                continue;
            }
            let t = Instant::now();
            let _ = cell.search_semantic_with_now(&mk_query(q), now, &sem_vec);
            vlat.push(t.elapsed().as_secs_f64() * 1000.0);
        }

        // per-mode quality by category; `graph_w` overrides the default
        // graph-list weight (v0.3 tuning: the third list must lift multi-salto
        // without displacing real text matches).
        let mut quality: Vec<(String, String, f64, f64, usize)> = Vec::new();
        let mut score_mode =
            |name: &str, mode: char, fusion: Option<Fusion>, graph_w: Option<f64>| {
                let categories = ["fattuali", "parafrasi", "multi-salto", "inglese"];
                for cat in categories {
                    let (recall, mrr, n) = {
                        let mut hits5 = 0_usize;
                        let mut mrr_sum = 0.0_f64;
                        let mut answered = 0_usize;
                        for q in &corpus.questions {
                            if q.expect_ids.is_empty() || q.category != cat {
                                continue;
                            }
                            let query = {
                                let mut query = mk_query(q);
                                if let Some(g) = graph_w {
                                    query.weights.graph_weight = g;
                                }
                                query
                            };
                            let results = match mode {
                                't' => cell
                                    .search_with_now(&query, now)
                                    .expect(/* valid */ "search"),
                                'v' => {
                                    cell.search_semantic_with_now(&query, now, &sem_vec)
                                        .expect(/* valid */ "search")
                                        .hits
                                }
                                _ => {
                                    let s = match fusion {
                                        Some(f) => sem.clone().with_fusion(f),
                                        None => sem.clone(),
                                    };
                                    cell.search_semantic_with_now(&query, now, &s)
                                        .expect(/* valid */ "search")
                                        .hits
                                }
                            };
                            let ids: Vec<String> =
                                results.iter().map(|h| h.note.id.to_string()).collect();
                            if let Some(rank) = ids.iter().position(|i| q.expect_ids.contains(i)) {
                                hits5 += 1;
                                mrr_sum += 1.0 / (rank + 1) as f64;
                            }
                            answered += 1;
                        }
                        (
                            if answered == 0 {
                                0.0
                            } else {
                                hits5 as f64 / answered as f64
                            },
                            if answered == 0 {
                                0.0
                            } else {
                                mrr_sum / answered as f64
                            },
                            answered,
                        )
                    };
                    quality.push((name.to_string(), cat.to_string(), recall, mrr, n));
                }
            };
        score_mode("testo", 't', None, None);
        // text mode with the graph list off: what the third list buys where
        // no vector can help
        score_mode("testo-g0", 't', None, Some(0.0));
        score_mode("vettori", 'v', None, None);
        score_mode("rrf", 'h', Some(Fusion::Rrf { k: 60 }), None);
        score_mode("rrf-g0.40", 'h', Some(Fusion::Rrf { k: 60 }), Some(0.4));
        let convex = |alpha: f64, norm: engramma_core::CosNorm| {
            Some(Fusion::Convex {
                alpha,
                cos_norm: norm,
            })
        };
        score_mode(
            "mm-0.3",
            'h',
            convex(0.3, engramma_core::CosNorm::Candidates),
            None,
        );
        score_mode(
            "mm-0.5",
            'h',
            convex(0.5, engramma_core::CosNorm::Candidates),
            None,
        );
        score_mode(
            "mm-0.7",
            'h',
            convex(0.7, engramma_core::CosNorm::Candidates),
            None,
        );
        // v0.3.2 (P2.10): the defaults, old and new, made explicit — the
        // table never carried its own default row. mm-0.6-g0.40 was the
        // v0.3.1 winner (best absolute weighted MRR); the robust rule of
        // this phase (worst case among the neighbours) moves the default to
        // mm-0.7-g0.35.
        score_mode(
            "mm-0.6-g0.40",
            'h',
            convex(0.6, engramma_core::CosNorm::Candidates),
            Some(0.4),
        );
        score_mode(
            "mm-0.7-g0.35",
            'h',
            convex(0.7, engramma_core::CosNorm::Candidates),
            Some(0.35),
        );
        // the tuning references (the full sweep is in REPORT §v0.3)
        for gw in [0.25_f64, 0.40] {
            let tag = format!("mm-0.7-g{gw:.2}");
            score_mode(
                &tag,
                'h',
                convex(0.7, engramma_core::CosNorm::Candidates),
                Some(gw),
            );
        }
        score_mode(
            "th-0.3",
            'h',
            convex(0.3, engramma_core::CosNorm::Theoretical),
            None,
        );
        score_mode(
            "th-0.5",
            'h',
            convex(0.5, engramma_core::CosNorm::Theoretical),
            None,
        );
        score_mode(
            "th-0.7",
            'h',
            convex(0.7, engramma_core::CosNorm::Theoretical),
            None,
        );

        SemanticReport {
            model,
            embed_notes_per_s,
            index_build_ms,
            index_mem_mb,
            graph_build_ms,
            graph_nodes,
            graph_edges,
            graph_mem_mb: graph_bytes as f64 / 1e6,
            graph_pushed_edges,
            text_hops_p50_ms,
            text_hops_p95_ms,
            vector_p50_ms: percentile(vlat.clone(), 0.5),
            vector_p95_ms: percentile(vlat.clone(), 0.95),
            hybrid_warm_p50_ms: percentile(warm.clone(), 0.5),
            hybrid_warm_p95_ms: percentile(warm.clone(), 0.95),
            hybrid_cold_p50_ms: percentile(cold.clone(), 0.5),
            hybrid_cold_p95_ms: percentile(cold.clone(), 0.95),
            quality,
        }
    });

    // file size (checkpoint the WAL first, like a rest after a bulk load),
    // then again after a compaction — v0.3.2 (P0.1): BOTH ways, and the bill
    // is the whole footprint (cell + WAL + snapshot files), before and
    // after. First WITH the file snapshot (v0.3.1 behavior), then a purge at
    // scale with the file present (the whole forgetting path: FTS optimize,
    // VACUUM, checkpoint, snapshot scrub), then the checkpoint compaction —
    // which replaces the snapshot row and deletes its file.
    cell.checkpoint()
        .expect(/* single bench writer */ "checkpoint");
    let file_bytes = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    let head: i64 = cell
        .debug_column("SELECT CAST(COALESCE(MAX(seq), 0) AS TEXT) FROM ops")
        .ok()
        .and_then(|v| v.first().and_then(|s| s.parse().ok()))
        .unwrap_or(0);
    let stem = db
        .file_name()
        .map(|n| n.to_string_lossy().trim_end_matches(".sqlite").to_string())
        .unwrap_or_default();
    let footprint = |dir: &std::path::Path| -> u64 {
        let mut total = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
        total += std::fs::metadata(dir.join(format!("{stem}.sqlite-wal")))
            .map(|m| m.len())
            .unwrap_or(0);
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with(&format!("{stem}-snap-")) {
                    total += e.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
        total
    };
    let compacted_ops = (head - 1000).max(0);
    let mut footprint_before = 0_u64;
    let mut footprint_after_keep = 0_u64;
    let mut footprint_after_plain = 0_u64;
    let mut keep_s = 0.0_f64;
    let mut plain_s = 0.0_f64;
    let mut purge_ms = 0.0_f64;
    if compacted_ops >= 1 {
        footprint_before = footprint(dir);
        let t = Instant::now();
        cell.compact(
            compacted_ops,
            &engramma_core::cell::CompactOptions {
                keep_snapshot: true,
            },
        )
        .expect(/* head is live */ "compact with snapshot");
        keep_s = t.elapsed().as_secs_f64();
        footprint_after_keep = footprint(dir);
        if let Some(id) = corpus.notes.first().map(|n| n.id) {
            let t = Instant::now();
            let _ = cell.purge(&id, "bench", "bench");
            purge_ms = t.elapsed().as_secs_f64() * 1000.0;
        }
        cell.checkpoint()
            .expect(/* single bench writer */ "checkpoint");
        let head2: i64 = cell
            .debug_column("SELECT CAST(COALESCE(MAX(seq), 0) AS TEXT) FROM ops")
            .ok()
            .and_then(|v| v.first().and_then(|s| s.parse().ok()))
            .unwrap_or(0);
        let t = Instant::now();
        cell.compact(
            (head2 - 1000).max(1),
            &engramma_core::cell::CompactOptions::default(),
        )
        .expect(/* head is live */ "checkpoint compact");
        plain_s = t.elapsed().as_secs_f64();
        footprint_after_plain = footprint(dir);
    }
    // the bench deletes its own cells: nothing may survive the run (the
    // prompt: the disk is nearly full)
    drop(cell);
    let stem = db
        .file_name()
        .map(|n| n.to_string_lossy().trim_end_matches(".sqlite").to_string())
        .unwrap_or_default();
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.join(format!("{stem}.sqlite{suffix}")));
    }
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(&format!("{stem}-snap-")) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }

    RunReport {
        variant: if uniform_updated_at {
            "uniform"
        } else {
            "viva"
        },
        notes: corpus.notes.len(),
        insert_notes_per_s: corpus.notes.len() as f64 / insert_s,
        open_ms,
        search_p50_ms: percentile(latencies.clone(), 0.5),
        search_p95_ms: percentile(latencies.clone(), 0.95),
        text_only_p50_ms: percentile(text_only, 0.5),
        text_hops_p50_ms,
        text_hops_p95_ms,
        recall_at_5: if answered == 0 {
            0.0
        } else {
            hits_at_5 as f64 / answered as f64
        },
        mrr: if answered == 0 {
            0.0
        } else {
            mrr_sum / answered as f64
        },
        questions: answered,
        file_bytes,
        footprint_before,
        footprint_after_keep,
        footprint_after_plain,
        compact_keep_s: keep_s,
        compact_plain_s: plain_s,
        purge_ms,
        compacted_ops,
        semantic,
    }
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    percentile(xs, 0.5)
}

/// Median of every metric over the single-run reports of one variant+size.
fn median_report(variant: &'static str, runs: &[RunReport]) -> RunReport {
    let pick = |f: fn(&RunReport) -> f64| median(runs.iter().map(f).collect());
    RunReport {
        variant,
        notes: runs[0].notes,
        footprint_before: runs.iter().map(|r| r.footprint_before).max().unwrap_or(0),
        footprint_after_keep: runs
            .iter()
            .map(|r| r.footprint_after_keep)
            .max()
            .unwrap_or(0),
        footprint_after_plain: runs
            .iter()
            .map(|r| r.footprint_after_plain)
            .max()
            .unwrap_or(0),
        compact_keep_s: pick(|r| r.compact_keep_s),
        compact_plain_s: pick(|r| r.compact_plain_s),
        purge_ms: pick(|r| r.purge_ms),
        compacted_ops: runs[0].compacted_ops,
        insert_notes_per_s: pick(|r| r.insert_notes_per_s),
        open_ms: pick(|r| r.open_ms),
        search_p50_ms: pick(|r| r.search_p50_ms),
        search_p95_ms: pick(|r| r.search_p95_ms),
        text_only_p50_ms: pick(|r| r.text_only_p50_ms),
        text_hops_p50_ms: pick(|r| r.text_hops_p50_ms),
        text_hops_p95_ms: pick(|r| r.text_hops_p95_ms),
        recall_at_5: pick(|r| r.recall_at_5),
        mrr: pick(|r| r.mrr),
        questions: runs[0].questions,
        file_bytes: runs.iter().map(|r| r.file_bytes).max().unwrap_or(0),
        semantic: median_semantic(runs),
    }
}

/// Median of the semantic scalars; the per-mode quality table comes from the
/// first run (deterministic corpus: the numbers do not move between runs).
fn median_semantic(runs: &[RunReport]) -> Option<SemanticReport> {
    let with_sem: Vec<&SemanticReport> = runs.iter().filter_map(|r| r.semantic.as_ref()).collect();
    if with_sem.is_empty() {
        return None;
    }
    let base = with_sem[0].clone();
    let pick = |f: fn(&SemanticReport) -> f64| median(with_sem.iter().map(|r| f(r)).collect());
    Some(SemanticReport {
        model: base.model,
        embed_notes_per_s: pick(|r| r.embed_notes_per_s),
        index_build_ms: pick(|r| r.index_build_ms),
        index_mem_mb: pick(|r| r.index_mem_mb),
        vector_p50_ms: pick(|r| r.vector_p50_ms),
        vector_p95_ms: pick(|r| r.vector_p95_ms),
        hybrid_warm_p50_ms: pick(|r| r.hybrid_warm_p50_ms),
        hybrid_warm_p95_ms: pick(|r| r.hybrid_warm_p95_ms),
        hybrid_cold_p50_ms: pick(|r| r.hybrid_cold_p50_ms),
        hybrid_cold_p95_ms: pick(|r| r.hybrid_cold_p95_ms),
        graph_build_ms: pick(|r| r.graph_build_ms),
        graph_nodes: with_sem[0].graph_nodes,
        graph_edges: with_sem[0].graph_edges,
        graph_mem_mb: pick(|r| r.graph_mem_mb),
        graph_pushed_edges: pick(|r| r.graph_pushed_edges),
        text_hops_p50_ms: pick(|r| r.text_hops_p50_ms),
        text_hops_p95_ms: pick(|r| r.text_hops_p95_ms),
        quality: base.quality,
    })
}

fn markdown(reports: &[RunReport], runs: usize) -> String {
    let mut out = format!(
        "# Benchmark Engramma v0.3\n\nGenerato da `engramma-bench` (seme fisso, corpus sintetico italiano; macchina: vedi REPORT.md). \
         Ogni riga è la **mediana di {runs} giri**. Le righe `sem-*` misurano il ramo vettoriale (corpus vivo). \
         Obiettivi del §11 del documento di progetto in fondo.\n\n\
         | corpus | note | inserimento (note/s) | apertura cella (ms) | ricerca p50 (ms) | ricerca p95 (ms) | solo testo p50 (ms) | testo+grafo p50 (ms) | testo+grafo p95 (ms) | richiamo@5 | MRR | domande | file (MB) | impronta prima (MB) | dopo compatt. base (MB) | dopo compatt. +istantanea (MB) | compact base (s) | compact +istantanea (s) | purge (ms) |\n\
         |---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n"
    );
    for r in reports {
        // v0.3.1 (P2.11): measured for every variant — the viva and uniform
        // rows printed 0.0 when these lived only in the semantic report
        let (tp50, tp95) = (r.text_hops_p50_ms, r.text_hops_p95_ms);
        out.push_str(&format!(
            "| {} | {} | {:.0} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.3} | {:.3} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.0} |\n",
            r.variant,
            r.notes,
            r.insert_notes_per_s,
            r.open_ms,
            r.search_p50_ms,
            r.search_p95_ms,
            r.text_only_p50_ms,
            tp50,
            tp95,
            r.recall_at_5,
            r.mrr,
            r.questions,
            r.file_bytes as f64 / 1e6,
            r.footprint_before as f64 / 1e6,
            r.footprint_after_plain as f64 / 1e6,
            r.footprint_after_keep as f64 / 1e6,
            r.compact_plain_s,
            r.compact_keep_s,
            r.purge_ms,
        ));
    }
    // semantic table
    let sem: Vec<&SemanticReport> = reports.iter().filter_map(|r| r.semantic.as_ref()).collect();
    if !sem.is_empty() {
        out.push_str(
            "\n## Ramo vettoriale (v0.2) e grafo in memoria (v0.3)\n\n\
             | modello | note | embedding (note/s) | costruzione indice (ms) | memoria indice (MB) | GraphIndex (ms) | nodi | archi | memoria grafo (MB) | archi spinti/domanda | vettori p50 (ms) | vettori p95 (ms) | ibrido caldo p50 (ms) | ibrido caldo p95 (ms) | ibrido con embedding p50 (ms) | ibrido con embedding p95 (ms) |\n\
             |---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n",
        );
        for r in reports {
            let Some(sr) = &r.semantic else { continue };
            out.push_str(&format!(
                "| {} | {} | {:.0} | {:.0} | {:.1} | {:.0} | {} | {} | {:.1} | {:.0} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |\n",
                sr.model,
                r.notes,
                sr.embed_notes_per_s,
                sr.index_build_ms,
                sr.index_mem_mb,
                sr.graph_build_ms,
                sr.graph_nodes,
                sr.graph_edges,
                sr.graph_mem_mb,
                sr.graph_pushed_edges,
                sr.vector_p50_ms,
                sr.vector_p95_ms,
                sr.hybrid_warm_p50_ms,
                sr.hybrid_warm_p95_ms,
                sr.hybrid_cold_p50_ms,
                sr.hybrid_cold_p95_ms,
            ));
        }
        // per-mode quality: one block per variant+size (the model matters
        // as much as the fusion — the v0.2.1 model choice needed both)
        out.push_str(
            "\nRichiamo@5 / MRR per modalità e categoria (domande generatrici, mediana dei giri):\n\n\
             | corpus | note | modalità | fattuali | parafrasi | multi-salto | inglese |\n\
             |---|---|---|---|---|---|---|\n",
        );
        for r in reports {
            let Some(sr) = &r.semantic else { continue };
            let modes = [
                "testo",
                "testo-g0",
                "vettori",
                "rrf",
                "rrf-g0.40",
                "mm-0.3",
                "mm-0.5",
                "mm-0.7",
                "mm-0.6-g0.40",
                "mm-0.7-g0.35",
                "mm-0.7-g0.25",
                "mm-0.7-g0.40",
            ];
            for m in modes {
                let mut cells = Vec::new();
                for cat in ["fattuali", "parafrasi", "multi-salto", "inglese"] {
                    let cell = sr
                        .quality
                        .iter()
                        .find(|(mode, c, _, _, _)| mode == m && c == cat)
                        .map(|(_, _, r5, mrr, n)| format!("{r5:.3}/{mrr:.3} ({n})"))
                        .unwrap_or_else(|| "—".into());
                    cells.push(cell);
                }
                out.push_str(&format!(
                    "| {} | {} | {} | {} |\n",
                    r.variant,
                    r.notes,
                    m,
                    cells.join(" | ")
                ));
            }
        }
    }
    out.push_str(
        "\nDue varianti per dimensione: **viva** — i fatti correnti hanno `updated_at` recente (ottobre 2026, come una memoria viva) e i riempitivi datano 2025, quindi il peso di recenza lavora; **uniform** — lo stesso `updated_at` per ogni nota, così il ranking è solo testo+grafo (quanto del richiamo dipendeva dalla recenza si vede dal confronto). \
         Nota: l'inserimento è una `put_note` per nota (una transazione ciascuna, fsync in WAL/NORMAL). La ricerca è testo+grafo (2 passi, come le domande multi-salto) con 5 risultati; «testo+grafo» è la stessa modalità testo con i salti accesi (il PPR ha sostituito la camminata SQL). Richiamo@5 e MRR sulle domande generatrici. \
         «Ibrido caldo»: la LRU delle domande tiene tutto il corpus (capacità = numero di domande), niente re-incorporazioni. «Impronta» è cella + WAL + file d'istantanea (v0.3.2: il conto onesto): «prima» è l'impronta subito prima della compattazione, «dopo compatt. base» segue la compattazione senza istantanea (punto di controllo, nessuna seconda copia), «dopo compatt. +istantanea» segue quella con --keep-snapshot (v0.3.1: stato logico, una sola vive); i payload sono dimenticati fino a 1000 operazioni dalla fine in entrambi i casi, e «purge» è una purga a corpus pieno con l'istantanea presente, FTS optimize e VACUUM compresi. \
         La varianza tra giri è riportata in REPORT.md (il primo giro a freddo è sempre il più lento: il file nasce e cresce).\n",
    );
    out
}

/// CLI configuration (unit-tested).
struct Args {
    targets: Vec<usize>,
    /// `None` = default location (`bench/RESULTS.md`).
    out: Option<PathBuf>,
    runs: usize,
    /// Run the profiling probe (fake embedder, no model) instead of the
    /// full benchmark.
    fake_embedder: bool,
    /// Diagnose the multi-hop questions (v0.2.1 P1.7): where the expected
    /// note ends up, its score and its graph path.
    multihop_debug: bool,
    /// Build a cell at the given size, then sustain `put_note` on it for a
    /// few seconds (v0.3.1, P2.9): run it under `sample` to see where a
    /// write's time goes at 100k.
    profile_insert: bool,
    /// How long the sustained write phase lasts (seconds).
    seconds: u64,
    /// The joint fusion × graph-weight calibration (v0.3.1, P2.10): the full
    /// grid at the given size, median of `runs` rounds, MRR weighted by the
    /// questions of each category; within 0.002 the paraphrases win.
    calibrate: bool,
    /// v0.4 (P1.6 diagnostics): the question-entity seeding at fixed
    /// weights, plus the two uncertainty signals per category.
    probe_seeding: bool,
}

fn parse_args(args: impl Iterator<Item = String>) -> Args {
    let mut a = Args {
        targets: Vec::new(),
        out: None,
        runs: 3,
        fake_embedder: false,
        multihop_debug: false,
        profile_insert: false,
        seconds: 15,
        calibrate: false,
        probe_seeding: false,
    };
    let mut it = args.peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--notes" => {
                if let Some(n) = it.next().and_then(|v| v.parse().ok()) {
                    a.targets.push(n);
                }
            }
            "--out" => a.out = it.next().map(PathBuf::from),
            "--runs" => {
                if let Some(r) = it.next().and_then(|v| v.parse::<usize>().ok()) {
                    a.runs = r.clamp(1, 9);
                }
            }
            "--fake-embedder" => a.fake_embedder = true,
            "--multihop-debug" => a.multihop_debug = true,
            "--profile-insert" => a.profile_insert = true,
            "--calibrate" => a.calibrate = true,
            "--probe-seeding" => a.probe_seeding = true,
            "--seconds" => {
                if let Some(v) = it.next().and_then(|v| v.parse::<u64>().ok()) {
                    a.seconds = v.clamp(1, 600);
                }
            }
            other => {
                if let Ok(n) = other.parse::<usize>() {
                    a.targets.push(n);
                }
            }
        }
    }
    a
}

fn load_model(name: &str) -> Option<std::sync::Arc<dyn Embedder>> {
    if std::env::var("ENGRAMMA_SKIP_MODEL_TESTS")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        eprintln!("ENGRAMMA_SKIP_MODEL_TESTS=1: niente benchmark semantico");
        return None;
    }
    match engramma_embed::Builtin::load_named(&engramma_embed::spec::model_root(), name) {
        Ok(b) => {
            eprintln!("modello {name} caricato");
            Some(std::sync::Arc::new(b))
        }
        Err(e) => {
            eprintln!("modello {name} non disponibile ({e}): salto le sue misure");
            None
        }
    }
}

fn graph_probe(target: usize, dir: &std::path::Path) {
    let corpus = build_corpus(target, false);
    let db = dir.join(format!("probe-{target}.sqlite"));
    let _ = std::fs::remove_file(&db);
    let cell = Cell::create(&db, "probe", "bench").expect(/* fresh */ "create");
    let t0 = Instant::now();
    for note in &corpus.notes {
        cell.put_note(note, "bench").expect(/* valid */ "put");
    }
    eprintln!(
        "insert: {} note in {:.1}s",
        corpus.notes.len(),
        t0.elapsed().as_secs_f64()
    );
    let (ms, (nodes, edges, bytes)) = cell.build_graph().expect(/* open */ "graph");
    eprintln!(
        "graph: {ms:.0} ms, {nodes} nodi, {edges} archi, {} MB",
        bytes as f64 / 1e6
    );
    cell.checkpoint().expect(/* writer */ "ckpt");
    let before = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    let head: i64 = cell
        .debug_column("SELECT CAST(COALESCE(MAX(seq), 0) AS TEXT) FROM ops")
        .ok()
        .and_then(|v| v.first().and_then(|s| s.parse().ok()))
        .unwrap_or(0);
    let t1 = Instant::now();
    cell.compact(
        head - 1000,
        &engramma_core::cell::CompactOptions {
            keep_snapshot: true,
        },
    )
    .expect(/* head */ "compact");
    cell.checkpoint().expect(/* writer */ "ckpt");
    let after = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    eprintln!(
        "compact (con istantanea): {:.1}s, file {} → {} MB (testa {head})",
        t1.elapsed().as_secs_f64(),
        before as f64 / 1e6,
        after as f64 / 1e6
    );
    assert!(cell.verify().expect(/* open */ "verify").ok());
    eprintln!("verify: ok");
    let _ = std::fs::remove_file(&db);
    let snap_name = format!(
        "{}-snap-{head}.sqlite",
        db.file_name()
            .map(|n| n.to_string_lossy().trim_end_matches(".sqlite").to_string())
            .unwrap_or_default()
    );
    if let Some(dir) = db.parent() {
        let _ = std::fs::remove_file(dir.join(snap_name));
    }
}

/// v0.3.1 (P2.9): where does a `put_note`'s time go at 100k? Build the
/// corpus cell (that phase IS the insert rate the report measures), then
/// sustain full replaces on the full cell for `seconds` while the operator
/// runs `sample` on the process. The cell is deleted at the end.
fn profile_insert(target: usize, seconds: u64, dir: &std::path::Path) {
    let corpus = build_corpus(target, false);
    let db = dir.join(format!("prof-{target}.sqlite"));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.join(format!("prof-{target}.sqlite{suffix}")));
    }
    let cell = Cell::create(&db, "prof", "bench").expect(/* fresh */ "create");
    let t0 = Instant::now();
    for note in &corpus.notes {
        cell.put_note(note, "bench").expect(/* valid */ "put");
    }
    eprintln!(
        "insert: {} note in {:.1}s ({:.0} note/s)",
        corpus.notes.len(),
        t0.elapsed().as_secs_f64(),
        corpus.notes.len() as f64 / t0.elapsed().as_secs_f64()
    );
    // sustained full replaces, round-robin over the corpus
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    let mut done: usize = 0;
    let mut i: usize = 0;
    while std::time::Instant::now() < deadline {
        let note = &corpus.notes[i % corpus.notes.len()];
        cell.put_note(note, "bench").expect(/* valid */ "put");
        done += 1;
        i += 1;
    }
    eprintln!(
        "sustain: {done} put_note in {seconds}s ({:.0}/s)",
        done as f64 / seconds as f64
    );
    drop(cell);
    // `ENGRAMMA_KEEP_CELL=1` leaves the built cell on disk: the REPORT's
    // manual proofs need a real 100k cell to run the CLI against (the
    // caller deletes it — the disk is nearly full).
    if std::env::var("ENGRAMMA_KEEP_CELL").is_err() {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(dir.join(format!("prof-{target}.sqlite{suffix}")));
        }
    }
}

/// v0.4 (P1.6, diagnostics): the question-entity seeding at FIXED weights
/// (the combination the grid never measured), plus the two uncertainty
/// signals per category — where the adaptive weight goes and why.
fn probe_seeding(target: usize, dir: &std::path::Path) {
    use std::sync::Arc;
    let corpus = build_corpus(target, false);
    let db = dir.join(format!("prob-{target}.sqlite"));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.join(format!("prob-{target}.sqlite{suffix}")));
    }
    let cell = Cell::create(&db, "prob", "bench").expect(/* fresh */ "create");
    for note in &corpus.notes {
        cell.put_note(note, "bench").expect(/* valid */ "put");
    }
    let embedder =
        load_model("granite-embedding-97m-multilingual-r2").expect(/* probe */ "97M");
    cell.add_model(embedder.model_id(), embedder.dim())
        .expect(/* valid */ "add");
    cell.set_query_cache_capacity(corpus.questions.len().max(1))
        .expect(/* open */ "lru");
    loop {
        let r = cell
            .embed_pending(embedder.as_ref(), 256)
            .expect(/* valid */ "embed");
        if r.done == 0 || r.remaining == 0 {
            break;
        }
    }
    let now = OffsetDateTime::from_unix_timestamp(1_810_000_000).expect(/* fixed */ "ts");
    let sem = Semantic::new(Arc::clone(&embedder));
    let categories = ["fattuali", "parafrasi", "multi-salto", "inglese"];
    let mrr_of = |seeding: engramma_core::GraphSeeding, gw: f64, alpha: f64| -> Vec<f64> {
        let sem = sem.clone().with_fusion(Fusion::Convex {
            alpha,
            cos_norm: engramma_core::CosNorm::Candidates,
        });
        let mut mrrs = vec![0.0; categories.len()];
        let mut counts = vec![0_usize; categories.len()];
        for q in &corpus.questions {
            if q.expect_ids.is_empty() {
                continue;
            }
            let ci = categories
                .iter()
                .position(|c| *c == q.category)
                .unwrap_or(usize::MAX);
            if ci == usize::MAX {
                continue;
            }
            let mut query = Query::text(&q.text).k(5).graph_hops(2);
            query.lang = Some(Lang::It);
            if let Some(a) = q.as_of {
                query.filters.as_of = Some(a);
            }
            query.weights.graph_seeding = seeding;
            query.weights.graph_weight = gw;
            let hits = cell
                .search_semantic_with_now(&query, now, &sem)
                .expect(/* valid */ "search")
                .hits;
            let ids: Vec<String> = hits.iter().map(|h| h.note.id.to_string()).collect();
            if let Some(rank) = ids.iter().position(|i| q.expect_ids.contains(i)) {
                mrrs[ci] += 1.0 / (rank + 1) as f64;
            }
            counts[ci] += 1;
        }
        for i in 0..mrrs.len() {
            mrrs[i] /= counts[i].max(1) as f64;
        }
        mrrs
    };
    println!("# Sonda: semina dalla domanda a peso FISSO ({target} note, 97M)\n");
    println!("| semina | α | g | fattuali | parafrasi | multi-salto | inglese | media |");
    println!("|---|---|---|---|---|---|---|---|");
    for (label, seeding) in [
        ("fusa", engramma_core::GraphSeeding::Fused),
        (
            "domanda",
            engramma_core::GraphSeeding::Question {
                seed_passage_weight: 0.05,
            },
        ),
    ] {
        for alpha in [0.6_f64, 0.7] {
            for gw in [0.30_f64, 0.35, 0.40, 0.45, 0.50] {
                let m = mrr_of(seeding, gw, alpha);
                println!(
                    "| {label} | {alpha:.1} | {gw:.2} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} |",
                    m[0],
                    m[1],
                    m[2],
                    m[3],
                    (m[0] + m[1] + m[2] + m[3]) / 4.0
                );
            }
        }
    }
    // the two signals, per category (hybrid, α 0.7): where the adaptive
    // weight would land and why
    println!("\n## I due segnali per categoria (primi 12 per categoria)\n");
    let sem70 = sem.clone().with_fusion(Fusion::Convex {
        alpha: 0.7,
        cos_norm: engramma_core::CosNorm::Candidates,
    });
    for cat in categories {
        println!("--- {cat}");
        let mut shown = 0;
        for q in &corpus.questions {
            if q.expect_ids.is_empty() || q.category != cat || shown >= 12 {
                continue;
            }
            shown += 1;
            let mut query = Query::text(&q.text).k(5).graph_hops(2);
            query.lang = Some(Lang::It);
            if let Some(a) = q.as_of {
                query.filters.as_of = Some(a);
            }
            query.weights.graph_seeding = engramma_core::GraphSeeding::Question {
                seed_passage_weight: 0.05,
            };
            let outcome = cell
                .search_semantic_with_now(&query, now, &sem70)
                .expect(/* valid */ "search");
            let hits = outcome.hits;
            let say = hits.first().and_then(|h| h.explain.graph.clone());
            let Some(say) = say else { continue };
            let rank = hits
                .iter()
                .position(|h| q.expect_ids.contains(&h.note.id.to_string()))
                .map(|r| format!("{}", r + 1))
                .unwrap_or_else(|| "—".into());
            println!(
                "  «{}» stacco {:.3} accordo {:?} peso {:.3} attesa al rango {}",
                q.text,
                say.gap.unwrap_or(0.0),
                say.agreement,
                say.weight,
                rank
            );
        }
    }
    drop(cell);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.join(format!("prob-{target}.sqlite{suffix}")));
    }
}

/// One configuration's per-category (recall, MRR, n) over a corpus.
#[allow(clippy::too_many_arguments)]
fn measure_config(
    cell: &Cell,
    corpus: &Corpus,
    now: OffsetDateTime,
    sem: &Semantic,
    categories: &[&'static str],
    fusion: Fusion,
    seeding: engramma_core::GraphSeeding,
    graph_weight: f64,
) -> Vec<(f64, f64, usize)> {
    let sem = sem.clone().with_fusion(fusion);
    let mut per_cat = Vec::new();
    for cat in categories {
        let mut hits5 = 0_usize;
        let mut mrr_sum = 0.0_f64;
        let mut answered = 0_usize;
        for q in &corpus.questions {
            if q.expect_ids.is_empty() || q.category != *cat {
                continue;
            }
            let mut query = Query::text(&q.text).k(5).graph_hops(2);
            query.lang = Some(Lang::It);
            if let Some(a) = q.as_of {
                query.filters.as_of = Some(a);
            }
            query.weights.graph_seeding = seeding;
            query.weights.graph_weight = graph_weight;
            let hits = cell
                .search_semantic_with_now(&query, now, &sem)
                .expect(/* valid */ "search")
                .hits;
            let ids: Vec<String> = hits.iter().map(|h| h.note.id.to_string()).collect();
            if let Some(rank) = ids.iter().position(|i| q.expect_ids.contains(i)) {
                hits5 += 1;
                mrr_sum += 1.0 / (rank + 1) as f64;
            }
            answered += 1;
        }
        per_cat.push((
            if answered == 0 {
                0.0
            } else {
                hits5 as f64 / answered as f64
            },
            if answered == 0 {
                0.0
            } else {
                mrr_sum / answered as f64
            },
            answered,
        ));
    }
    per_cat
}

/// Warm hybrid latencies (ms) over the whole question set: (p50, p95).
fn warm_latencies(
    cell: &Cell,
    corpus: &Corpus,
    now: OffsetDateTime,
    sem: &Semantic,
    seeding: engramma_core::GraphSeeding,
    graph_weight: f64,
) -> (f64, f64) {
    let mut ts: Vec<f64> = Vec::with_capacity(corpus.questions.len());
    for q in &corpus.questions {
        if q.expect_ids.is_empty() {
            continue;
        }
        let mut query = Query::text(&q.text).k(5).graph_hops(2);
        query.lang = Some(Lang::It);
        if let Some(a) = q.as_of {
            query.filters.as_of = Some(a);
        }
        query.weights.graph_seeding = seeding;
        query.weights.graph_weight = graph_weight;
        let t = Instant::now();
        let _ = cell.search_semantic_with_now(&query, now, sem);
        ts.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    (percentile(ts.clone(), 0.5), percentile(ts, 0.95))
}

/// v0.4 (P1.6): the ADAPTIVE calibration. The grid is the new graph's
/// knobs — `w_max` × `u_min` × `seed_passage_weight` × α (24 points) —
/// measured at 10k (median of `runs10k` rounds) and at 100k (one round on
/// the grid, the machine's honest budget), on the 97M. The criterion is
/// the declared one: the **mean of the per-category MRRs** (every category
/// counts the same), in the WORST of the measured sizes; within 0.002 the
/// configuration that keeps the paraphrases wins. The two fixed references
/// ride along (the v0.3.2 and v0.3.1 defaults, with the v0.3.2 seeding:
/// the like-for-like baseline). The winner and the references then get
/// `runs100k` rounds at the largest size, with warm hybrid latencies, and
/// the phase's thresholds are checked. Prints everything; deletes its
/// cells.
#[allow(clippy::too_many_lines)]
fn calibrate(sizes: &[usize], runs10k: usize, runs100k: usize, dir: &std::path::Path) {
    use std::sync::Arc;
    let categories = ["fattuali", "parafrasi", "multi-salto", "inglese"];
    let alphas = [0.6_f64, 0.7];
    let w_maxes = [0.40_f64, 0.45, 0.50, 0.60];
    let u_mins = [0.05_f64, 0.15];
    let spws = [0.05_f64, 0.15];
    #[derive(Clone, Copy)]
    struct P {
        alpha: f64,
        w_max: f64,
        u_min: f64,
        spw: f64,
    }
    let mut grid: Vec<P> = Vec::new();
    for &a in &alphas {
        for &w in &w_maxes {
            for &u in &u_mins {
                for &sp in &spws {
                    grid.push(P {
                        alpha: a,
                        w_max: w,
                        u_min: u,
                        spw: sp,
                    });
                }
            }
        }
    }
    // the fixed references: the old defaults, with the exact behavior of
    // their phase (fused seeding, fixed weight)
    let refs: [(String, Fusion, engramma_core::GraphSeeding, f64); 2] = [
        (
            "rif-α0.7-g0.35 (v0.3.2)".into(),
            Fusion::Convex {
                alpha: 0.7,
                cos_norm: engramma_core::CosNorm::Candidates,
            },
            engramma_core::GraphSeeding::Fused,
            0.35,
        ),
        (
            "rif-α0.6-g0.40 (v0.3.1)".into(),
            Fusion::Convex {
                alpha: 0.6,
                cos_norm: engramma_core::CosNorm::Candidates,
            },
            engramma_core::GraphSeeding::Fused,
            0.40,
        ),
    ];
    let config_of = |p: &P| -> (Fusion, engramma_core::GraphSeeding, f64) {
        (
            Fusion::Adaptive {
                alpha: p.alpha,
                cos_norm: engramma_core::CosNorm::Candidates,
                w_max: p.w_max,
                u_min: p.u_min,
            },
            engramma_core::GraphSeeding::Question {
                seed_passage_weight: p.spw,
            },
            p.w_max, // the Fixed-path weight; unused under Adaptive
        )
    };

    // per size: build once, measure the grid (+refs) `rounds` times, keep
    // the medians. The LARGEST size keeps its cell alive for the winner
    // phase.
    struct SizeResult {
        target: usize,
        per_config: Vec<Vec<(f64, f64, usize)>>,
    }
    let mut size_results: Vec<SizeResult> = Vec::new();
    let mut sizes_sorted = sizes.to_vec();
    sizes_sorted.sort_unstable();
    let largest = sizes_sorted.last().copied().unwrap_or(100_000);
    // the winner-phase world (filled at the largest size)
    /// The largest size's world, kept alive for the winner phase.
    struct Kept {
        cell: Cell,
        corpus: Corpus,
        sem: Semantic,
        now: OffsetDateTime,
    }
    let mut keep: Option<Kept> = None;
    for &target in &sizes_sorted {
        let rounds = if target <= 10_000 { runs10k } else { 1 };
        let corpus = build_corpus(target, false);
        let db = dir.join(format!("cal-{target}.sqlite"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(dir.join(format!("cal-{target}.sqlite{suffix}")));
        }
        let cell = Cell::create(&db, "cal", "bench").expect(/* fresh */ "create");
        for note in &corpus.notes {
            cell.put_note(note, "bench").expect(/* valid */ "put");
        }
        let embedder = load_model("granite-embedding-97m-multilingual-r2")
            .expect(/* the calibration runs on the default model */ "97M");
        cell.add_model(embedder.model_id(), embedder.dim())
            .expect(/* valid */ "add model");
        cell.set_query_cache_capacity(corpus.questions.len().max(1))
            .expect(/* open */ "lru");
        loop {
            let r = cell
                .embed_pending(embedder.as_ref(), 256)
                .expect(/* valid */ "embed");
            if r.done == 0 || r.remaining == 0 {
                break;
            }
        }
        let now = OffsetDateTime::from_unix_timestamp(1_810_000_000).expect(/* fixed */ "ts");
        let sem = Semantic::new(Arc::clone(&embedder));
        let n_configs = grid.len() + refs.len();
        let mut all_rounds: Vec<Vec<Vec<(f64, f64, usize)>>> = Vec::new();
        for round in 1..=rounds {
            eprintln!("taratura {target} note, giro {round}/{rounds}…");
            let mut one = Vec::with_capacity(n_configs);
            for p in &grid {
                let (fusion, seeding, gw) = config_of(p);
                one.push(measure_config(
                    &cell,
                    &corpus,
                    now,
                    &sem,
                    &categories,
                    fusion,
                    seeding,
                    gw,
                ));
            }
            for (_, fusion, seeding, gw) in &refs {
                one.push(measure_config(
                    &cell,
                    &corpus,
                    now,
                    &sem,
                    &categories,
                    *fusion,
                    *seeding,
                    *gw,
                ));
            }
            all_rounds.push(one);
        }
        let medians: Vec<Vec<(f64, f64, usize)>> = (0..n_configs)
            .map(|ci| {
                (0..categories.len())
                    .map(|cat| {
                        let mrrs: Vec<f64> = all_rounds.iter().map(|r| r[ci][cat].1).collect();
                        let recalls: Vec<f64> = all_rounds.iter().map(|r| r[ci][cat].0).collect();
                        let n = all_rounds[0][ci][cat].2;
                        (median(recalls), median(mrrs), n)
                    })
                    .collect()
            })
            .collect();
        size_results.push(SizeResult {
            target,
            per_config: medians,
        });
        if target == largest {
            keep = Some(Kept {
                cell,
                corpus,
                sem,
                now,
            });
        } else {
            drop(cell);
            for suffix in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(dir.join(format!("cal-{target}.sqlite{suffix}")));
            }
        }
    }

    // the criterion: mean per category, worst over the measured sizes
    let mean_of = |cats: &[(f64, f64, usize)]| -> f64 {
        category_mean(&cats.iter().map(|(_, m, _)| *m).collect::<Vec<_>>())
    };
    let mut rows: Vec<(usize, f64, Vec<f64>)> = Vec::new();
    for ci in 0..(grid.len() + refs.len()) {
        let means: Vec<f64> = size_results
            .iter()
            .map(|sr| mean_of(&sr.per_config[ci]))
            .collect();
        let worst = means.iter().copied().fold(f64::INFINITY, f64::min);
        rows.push((ci, worst, means.clone()));
    }
    let label = |ci: usize| -> String {
        if ci < grid.len() {
            let p = grid[ci];
            format!(
                "ad α{:.1} w{:.2} u{:.2} spw{:.2}",
                p.alpha, p.w_max, p.u_min, p.spw
            )
        } else {
            refs[ci - grid.len()].0.clone()
        }
    };
    println!("# Taratura adattiva (griglia w_max × u_min × spw × α, 97M, mediana dei giri)");
    println!(
        "criterio (v0.4 P1.6): MEDIA per categoria dell'MRR, nel caso peggiore tra le          dimensioni misurate; i due riferimenti in fondo sono i predefiniti vecchi          (seme fuso della v0.3.2)"
    );
    let sizes_hdr: Vec<String> = size_results
        .iter()
        .map(|sr| format!("media@{}k", sr.target / 1_000))
        .collect();
    println!(
        "| configurazione | {} | peggiore | fattuali | parafrasi | multi-salto | inglese |",
        sizes_hdr.join(" | ")
    );
    println!(
        "|---|{}|---|---|---|---|---|",
        "|---|".repeat(size_results.len())
    );
    for (ci, worst, means) in &rows {
        // per-category MRR at the WORST size (the one the criterion saw)
        let worst_si = means
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(0);
        let cats = &size_results[worst_si].per_config[*ci];
        let mean_str: Vec<String> = means.iter().map(|m| format!("{m:.4}")).collect();
        println!(
            "| {} | {} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} |",
            label(*ci),
            mean_str.join(" | "),
            worst,
            cats[0].1,
            cats[1].1,
            cats[2].1,
            cats[3].1
        );
    }
    // the winner among the ADAPTIVE points: best worst-mean, within 0.002
    // the one that keeps the paraphrases (at its worst size)
    let para_at_worst = |ci: usize| -> f64 {
        let means = rows
            .iter()
            .find(|(c, _, _)| *c == ci)
            .map(|(_, _, m)| m.clone())
            .unwrap_or_default();
        let worst_si = means
            .iter()
            .enumerate()
            .min_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(i, _)| i)
            .unwrap_or(0);
        size_results[worst_si].per_config[ci][1].1
    };
    let mut best_ci = 0_usize;
    let mut best_worst = f64::NEG_INFINITY;
    for (ci, worst, _) in &rows[..grid.len()] {
        let better = *worst > best_worst + 0.002
            || ((*worst - best_worst).abs() <= 0.002
                && para_at_worst(*ci) > para_at_worst(best_ci));
        if better {
            best_worst = *worst;
            best_ci = *ci;
        }
    }
    let p = grid[best_ci];
    println!(
        "\nvincitore della griglia: α = {}, w_max = {}, u_min = {}, spw = {} (media peggiore {best_worst:.4})",
        p.alpha, p.w_max, p.u_min, p.spw
    );

    // --- the winner phase at the largest size: median of `runs100k`
    // rounds for the winner and the references, plus warm hybrid latencies
    if let Some(Kept {
        cell,
        corpus,
        sem,
        now,
    }) = keep
    {
        eprintln!("fase del vincitore a {largest} note: {runs100k} giri + latenze…");
        let (w_fusion, w_seeding, w_gw) = config_of(&p);
        let finalists: [(String, Fusion, engramma_core::GraphSeeding, f64); 3] = [
            ("vincitore".into(), w_fusion, w_seeding, w_gw),
            refs[0].clone(),
            refs[1].clone(),
        ];
        let mut finals: Vec<Vec<Vec<(f64, f64, usize)>>> = Vec::new();
        for round in 1..=runs100k {
            eprintln!("giro {round}/{runs100k}…");
            let one: Vec<Vec<(f64, f64, usize)>> = finalists
                .iter()
                .map(|(_, f, s, g)| {
                    measure_config(&cell, &corpus, now, &sem, &categories, *f, *s, *g)
                })
                .collect();
            finals.push(one);
        }
        let medians: Vec<Vec<(f64, f64, usize)>> = (0..finalists.len())
            .map(|fi| {
                (0..categories.len())
                    .map(|cat| {
                        let mrrs: Vec<f64> = finals.iter().map(|r| r[fi][cat].1).collect();
                        let recalls: Vec<f64> = finals.iter().map(|r| r[fi][cat].0).collect();
                        (median(recalls), median(mrrs), finals[0][fi][cat].2)
                    })
                    .collect()
            })
            .collect();
        let lat: Vec<(f64, f64)> = finalists
            .iter()
            .map(|(_, _f, s, g)| warm_latencies(&cell, &corpus, now, &sem, *s, *g))
            .collect();
        println!("\n## Il vincitore e i riferimenti a {largest} note (mediana di {runs100k} giri)");
        println!(
            "| configurazione | fattuali | parafrasi | multi-salto | inglese | media | ibrido caldo p50/p95 (ms) |"
        );
        println!("|---|---|---|---|---|---|---|");
        for (fi, (name, _, _, _)) in finalists.iter().enumerate() {
            let mrrs: Vec<f64> = medians[fi].iter().map(|(_, m, _)| *m).collect();
            println!(
                "| {} | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {:.1} / {:.1} |",
                name,
                mrrs[0],
                mrrs[1],
                mrrs[2],
                mrrs[3],
                category_mean(&mrrs),
                lat[fi].0,
                lat[fi].1
            );
        }
        // the thresholds (spec P1.6), against the v0.3.2 reference
        let mrrs_of = |fi: usize| -> Vec<f64> { medians[fi].iter().map(|(_, m, _)| *m).collect() };
        let w = mrrs_of(0);
        let rif = mrrs_of(1);
        println!("\nSoglie della fase (a {largest}):");
        // categories in table order: fattuali(0) parafrasi(1) multi-salto(2) inglese(3)
        for (idx, name) in [(0_usize, "fattuali"), (1, "parafrasi"), (3, "inglese")] {
            println!(
                "- {name}: {:.4} contro {:.4} del riferimento (Δ {:+.4}, soglia −0,02: {})",
                w[idx],
                rif[idx],
                w[idx] - rif[idx],
                if w[idx] - rif[idx] >= -0.02 {
                    "OK"
                } else {
                    "NO"
                }
            );
        }
        println!(
            "- multi-salto: {:.4} (soglia ≥ 0,75: {})",
            w[2],
            if w[2] >= 0.75 { "OK" } else { "NO" }
        );
        let p95_delta = (lat[0].1 - lat[1].1) / lat[1].1.max(1e-9) * 100.0;
        println!(
            "- ibrido p95: {:.1} ms contro {:.1} ms del riferimento ({p95_delta:+.1}%, soglia +10%: {})",
            lat[0].1,
            lat[1].1,
            if p95_delta <= 10.0 { "OK" } else { "NO" }
        );
        drop(cell);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(dir.join(format!("cal-{largest}.sqlite{suffix}")));
        }
    }
}

fn main() {
    if std::env::var("ENGRAMMA_GRAPH_PROBE").is_ok() {
        let dir = std::env::temp_dir().join("engramma-bench");
        std::fs::create_dir_all(&dir).expect(/* temp */ "mkdir");
        graph_probe(100_000, &dir);
        graph_probe(10_000, &dir);
        return;
    }
    let args = parse_args(std::env::args().skip(1));
    let mut targets = args.targets;
    if targets.is_empty() {
        targets = vec![10_000, 100_000];
    }
    let dir = std::env::temp_dir().join("engramma-bench");
    std::fs::create_dir_all(&dir).expect(/* temp dir */ "mkdir");
    if args.multihop_debug {
        let mut sizes = targets.clone();
        if sizes.is_empty() {
            sizes = vec![10_000];
        }
        for t in &sizes {
            eprintln!("diagnosi multi-salto su {t} note…");
            multihop_debug(*t, 5, &dir);
        }
        return;
    }
    if args.probe_seeding {
        let t = targets.first().copied().unwrap_or(10_000);
        probe_seeding(t, &dir);
        return;
    }
    if args.calibrate {
        calibrate(&targets, args.runs, 3, &dir);
        return;
    }
    if args.profile_insert {
        let t = targets.first().copied().unwrap_or(100_000);
        profile_insert(t, args.seconds, &dir);
        return;
    }
    if args.fake_embedder {
        // The profiling probe: no corpus generator, no model — the engine
        // alone, one embed pass, vector and hybrid latencies, the int8 scan.
        let mut reports = Vec::new();
        for t in &targets {
            let mut runs: Vec<FakeReport> = Vec::with_capacity(args.runs);
            for i in 1..=args.runs {
                eprintln!("sonda {t} note, giro {i}/{}…", args.runs);
                runs.push(run_fake(*t, 30, &dir));
            }
            reports.push(median_fake(&runs));
        }
        let md = fake_markdown(&reports);
        print!("{md}");
        let out = args
            .out
            .clone()
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("PROBE.md"));
        std::fs::write(&out, &md).expect(/* target dir is writable */ "write report");
        eprintln!("scritto {}", out.display());
        return;
    }
    let mut reports = Vec::new();
    for t in &targets {
        for uniform in [false, true] {
            let mut runs: Vec<RunReport> = Vec::with_capacity(args.runs);
            for i in 1..=args.runs {
                eprintln!(
                    "benchmark {t} note, corpus {}, giro {i}/{}…",
                    if uniform { "uniform" } else { "viva" },
                    args.runs
                );
                runs.push(run(*t, &dir, uniform));
            }
            reports.push(median_report(
                if uniform { "uniform" } else { "viva" },
                &runs,
            ));
        }
        // ------------------------------------------------------------------
        // v0.2 semantic runs (live corpus; the uniform corpus only moves the
        // recency weight, which the semantic rerank no longer applies)
        // ------------------------------------------------------------------
        let m97 = load_model("granite-embedding-97m-multilingual-r2");
        if let Some(e) = &m97 {
            let mut runs: Vec<RunReport> = Vec::with_capacity(args.runs);
            for i in 1..=args.runs {
                eprintln!("benchmark semantico 97M {t} note, giro {i}/{}…", args.runs);
                runs.push(run_semantic(*t, &dir, false, Some(e)));
            }
            let mut r = median_report("sem-97M", &runs);
            if let Some(s) = &mut r.semantic {
                s.model = "97M".into();
            }
            reports.push(r);
        }
        // the 311M at 100k would take too long on this machine: 10k only,
        // declared here (prompt §5)
        if *t <= 10_000 {
            let m311 = load_model("granite-embedding-311m-multilingual-r2");
            if let Some(e) = &m311 {
                let mut runs: Vec<RunReport> = Vec::with_capacity(args.runs);
                for i in 1..=args.runs {
                    eprintln!("benchmark semantico 311M {t} note, giro {i}/{}…", args.runs);
                    runs.push(run_semantic(*t, &dir, false, Some(e)));
                }
                let mut r = median_report("sem-311M", &runs);
                if let Some(s) = &mut r.semantic {
                    s.model = "311M".into();
                }
                reports.push(r);
            }
        }
    }
    let md = markdown(&reports, args.runs);
    print!("{md}");
    // --out writes only there; without it the report lands in bench/RESULTS.md
    let out = args
        .out
        .clone()
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("RESULTS.md"));
    std::fs::write(&out, &md).expect(/* target dir is writable */ "write report");
    eprintln!("scritto {}", out.display());
}

/// The mean of the per-category MRRs (v0.4, P0.3/P1.6): every category
/// counts the same — the question-weighted MRR hid multi-salto, which is a
/// twentieth of the questions.
fn category_mean(mrrs: &[f64]) -> f64 {
    if mrrs.is_empty() {
        return 0.0;
    }
    mrrs.iter().sum::<f64>() / mrrs.len() as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_flags_and_defaults() {
        let a = parse_args(
            ["--notes", "10", "--notes", "20", "--out", "altrove.md"]
                .map(String::from)
                .into_iter(),
        );
        assert_eq!(a.targets, vec![10, 20]);
        assert_eq!(a.out.as_deref(), Some(std::path::Path::new("altrove.md")));
        assert_eq!(a.runs, 3);
        assert!(!a.fake_embedder);

        // bare numbers still work, and no --out means the default location
        let b = parse_args(std::iter::once("100000".to_string()));
        assert_eq!(b.targets, vec![100_000]);
        assert!(b.out.is_none());

        let c = parse_args(
            ["--runs", "1", "--notes", "5"]
                .map(String::from)
                .into_iter(),
        );
        assert_eq!(c.runs, 1);

        let d = parse_args(
            ["--fake-embedder", "--notes", "1000", "--runs", "2"]
                .map(String::from)
                .into_iter(),
        );
        assert!(d.fake_embedder);
        assert_eq!(d.targets, vec![1000]);
        assert_eq!(d.runs, 2);

        let e = parse_args(
            ["--multihop-debug", "--notes", "10000"]
                .map(String::from)
                .into_iter(),
        );
        assert!(e.multihop_debug);
        assert_eq!(e.targets, vec![10_000]);

        let f = parse_args(
            ["--calibrate", "--notes", "100000", "--runs", "3"]
                .map(String::from)
                .into_iter(),
        );
        assert!(f.calibrate);
        assert_eq!(f.targets, vec![100_000]);
        assert_eq!(f.runs, 3);
    }

    #[test]
    fn category_mean_counts_every_category_the_same() {
        // multi-salto is 5% of the questions: weighted it disappears,
        // the mean does not
        let mrrs = [0.9, 0.9, 0.1, 0.9];
        assert!((category_mean(&mrrs) - 0.7).abs() < 1e-12);
        assert_eq!(category_mean(&[]), 0.0);
    }
}
