// SPDX-License-Identifier: Apache-2.0

//! `engramma-bench`: deterministic synthetic memory (fixed seed), then
//! insert / search / open / recall measurements (§11). Run in `--release`.
//!
//! ```text
//! engramma-bench [--notes 10000] [--notes 100000] [--out bench/RESULTS.md]
//! ```

use std::path::PathBuf;
use std::time::Instant;

use engramma_core::format::{At, Lang, Note, NoteType};
use engramma_core::search::Query;
use engramma_core::{Cell, Confidence, Status};
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
/// with changes over time, plus questions with expected answers.
fn build_corpus(target: usize) -> Corpus {
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
            0 => questions.push(Question {
                text: format!("dove abita {name}"),
                as_of: None,
                expect_ids: vec![cur_id.to_string(), person_id.to_string()],
            }),
            1 => questions.push(Question {
                text: format!("dove lavora {name}"),
                as_of: None,
                expect_ids: vec![cur_work.to_string(), person_id.to_string()],
            }),
            2 => questions.push(Question {
                text: format!("dove abitava {name}"),
                as_of: Some(validity(2022, 6, 15).start()),
                expect_ids: vec![old_id.to_string()],
            }),
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
                });
            }
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
}

fn run(target: usize, dir: &std::path::Path) -> RunReport {
    let corpus = build_corpus(target);
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

    // file size (checkpoint the WAL first, like a rest after a bulk load)
    cell.checkpoint()
        .expect(/* single bench writer */ "checkpoint");
    drop(cell);
    let file_bytes = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(&db);

    RunReport {
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
    }
}

fn markdown(reports: &[RunReport]) -> String {
    let mut out = String::from(
        "# Benchmark Engramma v0.1\n\nGenerato da `engramma-bench` (seme fisso, corpus sintetico italiano; macchina: vedi REPORT.md). \
         Obiettivi del §11 del documento di progetto in fondo.\n\n\
         | note | inserimento (note/s) | apertura cella (ms) | ricerca p50 (ms) | ricerca p95 (ms) | solo testo p50 (ms) | richiamo@5 | MRR | domande | file (MB) |\n\
         |---|---|---|---|---|---|---|---|---|---|\n",
    );
    for r in reports {
        out.push_str(&format!(
            "| {} | {:.0} | {:.1} | {:.1} | {:.1} | {:.1} | {:.3} | {:.3} | {} | {:.1} |\n",
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
    out.push_str(
        "\nNel corpus i fatti correnti hanno `updated_at` recente (ottobre 2026, come una memoria viva) e i riempitivi datano 2025. Nota: l'inserimento è una `put_note` per nota (una transazione ciascuna, fsync in WAL/NORMAL). \
         La ricerca è testo+grafo (2 passi, come le domande multi-salto) con 5 risultati. Richiamo@5 e MRR sulle domande generatrici.\n",
    );
    out
}

fn main() {
    let mut targets: Vec<usize> = Vec::new();
    for arg in std::env::args().skip(1) {
        if let Ok(n) = arg.parse::<usize>() {
            targets.push(n);
        }
    }
    if targets.is_empty() {
        targets = vec![10_000, 100_000];
    }
    let dir = std::env::temp_dir().join("engramma-bench");
    std::fs::create_dir_all(&dir).expect(/* temp dir */ "mkdir");
    let mut reports = Vec::new();
    for t in &targets {
        eprintln!("benchmark con {t} note…");
        reports.push(run(*t, &dir));
    }
    let md = markdown(&reports);
    print!("{md}");
    let out = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("RESULTS.md");
    std::fs::write(&out, &md).expect(/* crate dir is writable */ "write RESULTS.md");
    eprintln!("scritto {}", out.display());
}
