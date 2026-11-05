// SPDX-License-Identifier: Apache-2.0

//! `engramma-bench`: deterministic synthetic memory (fixed seed), then
//! insert / search / open / recall measurements (§11). Run in `--release`.
//!
//! ```text
//! engramma-bench [--notes 10000] [--notes 100000] [--out PATH.md] [--runs 3]
//! ```
//!
//! `--notes` is repeatable (bare numbers also work). Every size runs `--runs`
//! times (default 3) and the table reports the **median** of each metric,
//! for two corpus variants: **viva** (current facts carry a recent
//! `updated_at`, like a living memory — the recency weight matters) and
//! **uniform** (the same `updated_at` for every note — pure text+graph
//! ranking). Without `--out` the report goes to `bench/RESULTS.md`; with it,
//! only there (RESULTS.md is left alone).

use std::path::PathBuf;
use std::time::Instant;

use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::search::{Fusion, Mode, Query, Semantic};
use engramma_core::{Cell, Confidence, Embedder, Status};
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
    // person index -> (name, city_current, company, partner_idx)
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
            if rng.below(3) == 0 {
                Some(rng.below(n_people as u64) as usize)
            } else {
                None
            },
        ));
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
        let person_note = base_note(
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
        notes.push(base_note(
            id,
            NoteType::Fact,
            format!("Appunto {m} su {name}"),
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
    recall_at_5: f64,
    mrr: f64,
    questions: usize,
    file_bytes: u64,
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

    // search — full pipeline, and text-only (graph_hops = 0) for profiling
    let now =
        OffsetDateTime::from_unix_timestamp(1_810_000_000).expect(/* fixed valid ts */ "ts");
    let mut text_only: Vec<f64> = Vec::new();
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
    }
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
        for q in &corpus.questions {
            if q.expect_ids.is_empty() {
                continue;
            }
            let t = Instant::now();
            let _ = cell.search_semantic_with_now(&mk_query(q), now, &sem);
            warm.push(t.elapsed().as_secs_f64() * 1000.0);
        }
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

        // per-mode quality by category
        let mut quality: Vec<(String, String, f64, f64, usize)> = Vec::new();
        let mut score_mode = |name: &str, mode: char, fusion: Option<Fusion>| {
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
                        let results = match mode {
                            't' => cell
                                .search_with_now(&mk_query(q), now)
                                .expect(/* valid */ "search"),
                            'v' => {
                                cell.search_semantic_with_now(&mk_query(q), now, &sem_vec)
                                    .expect(/* valid */ "search")
                                    .hits
                            }
                            _ => {
                                let s = match fusion {
                                    Some(f) => sem.clone().with_fusion(f),
                                    None => sem.clone(),
                                };
                                cell.search_semantic_with_now(&mk_query(q), now, &s)
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
        score_mode("testo", 't', None);
        score_mode("vettori", 'v', None);
        score_mode("rrf", 'h', Some(Fusion::Rrf { k: 60 }));
        score_mode("convex-0.3", 'h', Some(Fusion::Convex { alpha: 0.3 }));
        score_mode("convex-0.5", 'h', Some(Fusion::Convex { alpha: 0.5 }));
        score_mode("convex-0.7", 'h', Some(Fusion::Convex { alpha: 0.7 }));

        SemanticReport {
            model,
            embed_notes_per_s,
            index_build_ms,
            index_mem_mb,
            vector_p50_ms: percentile(vlat.clone(), 0.5),
            vector_p95_ms: percentile(vlat.clone(), 0.95),
            hybrid_warm_p50_ms: percentile(warm.clone(), 0.5),
            hybrid_warm_p95_ms: percentile(warm.clone(), 0.95),
            hybrid_cold_p50_ms: percentile(cold.clone(), 0.5),
            hybrid_cold_p95_ms: percentile(cold.clone(), 0.95),
            quality,
        }
    });

    // file size (checkpoint the WAL first, like a rest after a bulk load)
    cell.checkpoint()
        .expect(/* single bench writer */ "checkpoint");
    drop(cell);
    let file_bytes = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(&db);

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
        insert_notes_per_s: pick(|r| r.insert_notes_per_s),
        open_ms: pick(|r| r.open_ms),
        search_p50_ms: pick(|r| r.search_p50_ms),
        search_p95_ms: pick(|r| r.search_p95_ms),
        text_only_p50_ms: pick(|r| r.text_only_p50_ms),
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
        quality: base.quality,
    })
}

fn markdown(reports: &[RunReport], runs: usize) -> String {
    let mut out = format!(
        "# Benchmark Engramma v0.2\n\nGenerato da `engramma-bench` (seme fisso, corpus sintetico italiano; macchina: vedi REPORT.md). \
         Ogni riga è la **mediana di {runs} giri**. Le righe `sem-*` misurano il ramo vettoriale (corpus vivo). \
         Obiettivi del §11 del documento di progetto in fondo.\n\n\
         | corpus | note | inserimento (note/s) | apertura cella (ms) | ricerca p50 (ms) | ricerca p95 (ms) | solo testo p50 (ms) | richiamo@5 | MRR | domande | file (MB) |\n\
         |---|---|---|---|---|---|---|---|---|---|---|\n"
    );
    for r in reports {
        out.push_str(&format!(
            "| {} | {} | {:.0} | {:.1} | {:.1} | {:.1} | {:.1} | {:.3} | {:.3} | {} | {:.1} |\n",
            r.variant,
            r.notes,
            r.insert_notes_per_s,
            r.open_ms,
            r.search_p50_ms,
            r.search_p95_ms,
            r.text_only_p50_ms,
            r.recall_at_5,
            r.mrr,
            r.questions,
            r.file_bytes as f64 / 1e6,
        ));
    }
    // semantic table
    let sem: Vec<&SemanticReport> = reports.iter().filter_map(|r| r.semantic.as_ref()).collect();
    if !sem.is_empty() {
        out.push_str(
            "\n## Ramo vettoriale (v0.2)\n\n\
             | modello | note | embedding (note/s) | costruzione indice (ms) | memoria indice (MB) | vettori p50 (ms) | vettori p95 (ms) | ibrido caldo p50 (ms) | ibrido caldo p95 (ms) | ibrido con embedding p50 (ms) | ibrido con embedding p95 (ms) |\n\
             |---|---|---|---|---|---|---|---|---|---|---|\n",
        );
        for r in reports {
            let Some(sr) = &r.semantic else { continue };
            out.push_str(&format!(
                "| {} | {} | {:.0} | {:.0} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |\n",
                sr.model,
                r.notes,
                sr.embed_notes_per_s,
                sr.index_build_ms,
                sr.index_mem_mb,
                sr.vector_p50_ms,
                sr.vector_p95_ms,
                sr.hybrid_warm_p50_ms,
                sr.hybrid_warm_p95_ms,
                sr.hybrid_cold_p50_ms,
                sr.hybrid_cold_p95_ms,
            ));
        }
        // per-mode quality (first semantic report per size)
        out.push_str(
            "\nRichiamo@5 / MRR per modalità e categoria (domande generatrici, mediana dei giri):\n\n\
             | note | modalità | fattuali | parafrasi | multi-salto | inglese |\n\
             |---|---|---|---|---|---|\n",
        );
        let mut seen_sizes: Vec<usize> = Vec::new();
        for r in reports {
            let Some(sr) = &r.semantic else { continue };
            if seen_sizes.contains(&r.notes) {
                continue;
            }
            seen_sizes.push(r.notes);
            let modes = [
                "testo",
                "vettori",
                "rrf",
                "convex-0.3",
                "convex-0.5",
                "convex-0.7",
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
                    "| {} | {} | {} |\n",
                    r.notes,
                    m,
                    cells.join(" | ")
                ));
            }
        }
    }
    out.push_str(
        "\nDue varianti per dimensione: **viva** — i fatti correnti hanno `updated_at` recente (ottobre 2026, come una memoria viva) e i riempitivi datano 2025, quindi il peso di recenza lavora; **uniform** — lo stesso `updated_at` per ogni nota, così il ranking è solo testo+grafo (quanto del richiamo dipendeva dalla recenza si vede dal confronto). \
         Nota: l'inserimento è una `put_note` per nota (una transazione ciascuna, fsync in WAL/NORMAL). La ricerca è testo+grafo (2 passi, come le domande multi-salto) con 5 risultati. Richiamo@5 e MRR sulle domande generatrici. \
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
}

fn parse_args(args: impl Iterator<Item = String>) -> Args {
    let mut a = Args {
        targets: Vec::new(),
        out: None,
        runs: 3,
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

fn main() {
    let args = parse_args(std::env::args().skip(1));
    let mut targets = args.targets;
    if targets.is_empty() {
        targets = vec![10_000, 100_000];
    }
    let dir = std::env::temp_dir().join("engramma-bench");
    std::fs::create_dir_all(&dir).expect(/* temp dir */ "mkdir");
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
    }
}
