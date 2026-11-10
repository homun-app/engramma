// SPDX-License-Identifier: Apache-2.0

//! v0.4.4 (P3): LongMemEval-S (cleaned) as an external, comparable number
//! (DESIGN §11). One cell per question, in a temp directory deleted at the
//! end; note = round (a `user` turn plus the `assistant` turn that follows,
//! the granularity of the original work), `createdAt` = the session date,
//! `source = longmemeval`, `session_id` and round index in the `x-lme-*`
//! extensions. A round is turn-level relevant when one of its turns has
//! `has_answer: true`.
//!
//! The official metrics of `src/retrieval/eval_utils.py` (LongMemEval),
//! skipping the 30 `_abs` questions: `recall_any@k`, `recall_all@k` and
//! binary `ndcg@k` (dcg = r[0] + Σ r[i]/log2(i+1), ideal = the relevances
//! sorted descending, 0 when the ideal is 0), at SESSION level
//! (`evaluate_retrieval_turn2session`: the ranked rounds reduce to distinct
//! sessions in order, until k of them or the list ends) and at TURN level,
//! k = 5 and 10, averaged over the questions and per `question_type`. The
//! question text is searched with the `question_date` as `now`, so the
//! recency weight sees 2023 dates; the ranked list holds the top 50 rounds
//! (session-distinct @10 needs more than 10 raw hits).
//!
//! Modes: `text` (BM25, `graph_hops = 0`) and `hybrid` (the engine
//! defaults). The vector cache (`~/.cache/engramma/bench/`, one record
//! `sha256(text) ‖ vector` per round text) makes every distinct round embed
//! once for ALL the questions: the first hybrid run pays the model, a
//! second costs minutes and must reproduce the same numbers.
//!
//! ```text
//! cargo run -p engramma-bench --release --example longmemeval -- \
//!          [--mode text|hybrid|both] [--subset N] [--data PATH] [--out PATH.md]
//! ```
//!
//! `--subset N` (seed 42, proportional per `question_type`, ids printed)
//! runs BOTH modes on the same questions, for a like-for-like comparison.
//! No thresholds here: this run fixes the starting number. The data never
//! enters the repo; the sha256 of the file is checked first.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use engramma_core::format::{At, Lang, Note, NoteType, YamlValue};
use engramma_core::search::{Hit, Query, Semantic};
use engramma_core::{Cell, Confidence, EmbedError, Embedder, Status, TextKind};
use serde::Deserialize;
use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};
use ulid::Ulid;

const EXPECTED_SHA256: &str =
    "d6f21ea9d60a0d56f34a05b609c79c88a451d2ae03597821ea3d5a9678c3a442";
/// Ranked rounds per question: session-distinct @10 needs more than 10
/// raw hits (the original walks its retrieved list the same way).
const TOP_K: usize = 50;
const KS: [usize; 2] = [5, 10];
const SUBSET_SEED: u64 = 42;

fn die(what: impl std::fmt::Display) -> ! {
    eprintln!("longmemeval: {what}");
    std::process::exit(1);
}

// ---------------------------------------------------------------------------
// the dataset
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Turn {
    role: String,
    content: String,
    #[serde(default)]
    has_answer: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct Question {
    question_id: String,
    question_type: String,
    question: String,
    question_date: String,
    answer_session_ids: Vec<String>,
    haystack_session_ids: Vec<String>,
    haystack_dates: Vec<String>,
    haystack_sessions: Vec<Vec<Turn>>,
}

/// One round: what a note is made of.
#[derive(Debug)]
struct Round {
    session: String,
    body: String,
    at: OffsetDateTime,
    relevant: bool,
}

/// `2023/05/20 (Sat) 02:21` → UTC (the dataset dates are naive).
fn parse_when(text: &str) -> OffsetDateTime {
    let bad = || format!("unparseable date `{text}`");
    let mut parts = text.split_whitespace();
    let date = parts.next().unwrap_or_else(|| die(bad()));
    let _dow = parts.next();
    let clock = parts.next().unwrap_or_else(|| die(bad()));
    let num = |s: Option<&str>| -> i64 {
        s.unwrap_or_else(|| die(bad()))
            .parse()
            .unwrap_or_else(|_| die(bad()))
    };
    let mut d = date.split('/');
    let y = num(d.next()) as i32;
    let m = num(d.next()) as u8;
    let day = num(d.next()) as u8;
    let mut c = clock.split(':');
    let h = num(c.next()) as u8;
    let min = num(c.next()) as u8;
    let month = Month::try_from(m).unwrap_or_else(|_| die(bad()));
    let date = Date::from_calendar_date(y, month, day).unwrap_or_else(|_| die(bad()));
    let time = Time::from_hms(h, min, 0).unwrap_or_else(|_| die(bad()));
    PrimitiveDateTime::new(date, time).assume_utc()
}

fn rounds_of(q: &Question) -> Vec<Round> {
    let mut rounds = Vec::new();
    for (s, session) in q.haystack_sessions.iter().enumerate() {
        let session_id = q.haystack_session_ids[s].clone();
        let at = parse_when(&q.haystack_dates[s]);
        let mut i = 0;
        while i < session.len() {
            if session[i].role != "user" {
                i += 1;
                continue;
            }
            let user = &session[i];
            let assistant = session.get(i + 1).filter(|t| t.role == "assistant");
            let body = match assistant {
                Some(a) => format!("{}\n\n{}", user.content, a.content),
                None => user.content.clone(),
            };
            let relevant = user.has_answer == Some(true)
                || assistant.is_some_and(|a| a.has_answer == Some(true));
            rounds.push(Round {
                session: session_id.clone(),
                body,
                at,
                relevant,
            });
            i += 1 + usize::from(assistant.is_some());
        }
    }
    rounds
}

// ---------------------------------------------------------------------------
// the vector cache: one record `sha256(text) ‖ f32×dim` per round text
// ---------------------------------------------------------------------------

fn sha256(text: &str) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    let bytes: &[u8] = digest.as_ref();
    let mut key = [0_u8; 32];
    key.copy_from_slice(&bytes[..32]);
    key
}

struct VectorCache {
    file: Mutex<Option<std::fs::File>>,
    dim: usize,
    /// key → byte offset of the record (`32 + 4·dim` bytes each).
    index: Mutex<HashMap<[u8; 32], u64>>,
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub embedded_chars: AtomicU64,
}

impl VectorCache {
    fn open(path: &Path, dim: usize) -> Self {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|e| die(format!("cannot create {}: {e}", parent.display())));
        }
        let record = 32 + 4 * dim;
        let mut index = HashMap::new();
        if let Ok(bytes) = std::fs::read(path) {
            if bytes.len() % record != 0 {
                die(format!(
                    "{}: {} bytes is not a whole number of {record}-byte records",
                    path.display(),
                    bytes.len()
                ));
            }
            for (i, chunk) in bytes.chunks_exact(record).enumerate() {
                let mut key = [0_u8; 32];
                key.copy_from_slice(&chunk[..32]);
                index.insert(key, (i * record) as u64);
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap_or_else(|e| die(format!("cannot open {}: {e}", path.display())));
        Self {
            file: Mutex::new(Some(file)),
            dim,
            index: Mutex::new(index),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            embedded_chars: AtomicU64::new(0),
        }
    }

    fn get(&self, text: &str) -> Option<Vec<f32>> {
        let key = sha256(text);
        let offset = *self.index.lock().ok()?.get(&key)?;
        let record = 32 + 4 * self.dim;
        let mut buf = vec![0_u8; record];
        {
            let mut slot = self.file.lock().ok()?;
            let file = slot.as_mut()?;
            use std::io::{Read as _, Seek as _, SeekFrom};
            if file
                .seek(SeekFrom::Start(offset))
                .and_then(|_| file.read_exact(&mut buf))
                .is_err()
            {
                return None;
            }
        }
        let mut out = Vec::with_capacity(self.dim);
        for i in 0..self.dim {
            let at = 32 + i * 4;
            let bytes: [u8; 4] = buf[at..at + 4].try_into().ok()?;
            out.push(f32::from_le_bytes(bytes));
        }
        Some(out)
    }

    fn put(&self, text: &str, vector: &[f32], chars: u64) {
        if vector.len() != self.dim {
            die(format!(
                "cache: {}-dim vector for a {}-dim model",
                vector.len(),
                self.dim
            ));
        }
        let key = sha256(text);
        let mut buf = Vec::with_capacity(32 + 4 * self.dim);
        buf.extend_from_slice(&key);
        for v in vector {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        use std::io::Write as _;
        let mut slot = match self.file.lock() {
            Ok(f) => f,
            Err(_) => return,
        };
        let Some(file) = slot.as_mut() else { return };
        let Ok(offset) = file.stream_position() else {
            return;
        };
        if file.write_all(&buf).and_then(|_| file.flush()).is_ok() {
            if let Ok(mut index) = self.index.lock() {
                index.insert(key, offset);
            }
            self.embedded_chars.fetch_add(chars, Ordering::Relaxed);
        }
    }

    fn records(&self) -> usize {
        self.index.lock().map_or(0, |i| i.len())
    }
}

/// The builtin embedder behind the on-disk cache: each DISTINCT round text
/// embeds once, across questions and runs; queries go straight through.
struct CachedBuiltin {
    inner: engramma_embed::Builtin,
    cache: Arc<VectorCache>,
}

impl Embedder for CachedBuiltin {
    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    fn max_tokens(&self) -> usize {
        self.inner.max_tokens()
    }

    fn embed(&self, texts: &[&str], kind: TextKind) -> Result<Vec<Vec<f32>>, EmbedError> {
        if kind == TextKind::Query {
            return self.inner.embed(texts, kind);
        }
        let mut out: Vec<Option<Vec<f32>>> = Vec::with_capacity(texts.len());
        let mut missing: Vec<(usize, &str)> = Vec::new();
        for (i, text) in texts.iter().enumerate() {
            match self.cache.get(text) {
                Some(v) => {
                    self.cache.hits.fetch_add(1, Ordering::Relaxed);
                    out.push(Some(v));
                }
                None => {
                    self.cache.misses.fetch_add(1, Ordering::Relaxed);
                    out.push(None);
                    missing.push((i, text));
                }
            }
        }
        if !missing.is_empty() {
            let fresh_texts: Vec<&str> = missing.iter().map(|(_, t)| *t).collect();
            let fresh = self.inner.embed(&fresh_texts, kind)?;
            if fresh.len() != missing.len() {
                return Err(EmbedError::BadModel(format!(
                    "embedded {} of {} texts",
                    fresh.len(),
                    missing.len()
                )));
            }
            for ((i, text), vector) in missing.iter().zip(fresh) {
                let chars = text.chars().count() as u64;
                self.cache.put(text, &vector, chars);
                out[*i] = Some(vector);
            }
        }
        Ok(out.into_iter().map(|v| v.unwrap_or_default()).collect())
    }

    fn approx_tokens(&self, text: &str) -> usize {
        self.inner.approx_tokens(text)
    }
}

// ---------------------------------------------------------------------------
// the official metrics (eval_utils.py), binary relevance
// ---------------------------------------------------------------------------

/// `dcg(r, k) = r[0] + Σ_{i≥1} r[i]/log2(i+1)` over the first k ranks.
fn dcg(rels: &[bool], k: usize) -> f64 {
    let mut d = 0.0_f64;
    for (i, r) in rels.iter().take(k).enumerate() {
        if !*r {
            continue;
        }
        d += if i == 0 {
            1.0
        } else {
            1.0 / ((i + 1) as f64).log2()
        };
    }
    d
}

#[derive(Debug, Default, Clone, Copy)]
struct Metrics {
    recall_any: [f64; 2],
    recall_all: [f64; 2],
    ndcg: [f64; 2],
    n: u64,
    /// Questions with at least one relevant item (the denominators of
    /// `recall_all` and `ndcg`); the non-abs questions all have one, but
    /// the code does not assume it.
    n_rel: u64,
}

impl Metrics {
    fn add_ranking(&mut self, ranked: &[bool], relevant_total: usize) {
        self.n += 1;
        for (ki, k) in KS.iter().enumerate() {
            let top = &ranked[..(*k).min(ranked.len())];
            if top.iter().any(|&r| r) {
                self.recall_any[ki] += 1.0;
            }
            if relevant_total == 0 {
                continue;
            }
            let found = top.iter().filter(|&&r| r).count();
            if found == relevant_total {
                self.recall_all[ki] += 1.0;
            }
            let ideal = dcg(&vec![true; relevant_total], *k);
            self.ndcg[ki] += if ideal > 0.0 { dcg(top, *k) / ideal } else { 1.0 };
        }
        if relevant_total > 0 {
            self.n_rel += 1;
        }
    }

    fn mean(&self, v: &[f64; 2], ki: usize) -> f64 {
        v[ki] / self.n as f64
    }

    fn cells(&self) -> String {
        let mut out = String::new();
        for ki in 0..KS.len() {
            let _ = write!(
                out,
                "| {:.4} | {:.4} | {:.4} ",
                self.mean(&self.recall_any, ki),
                self.mean(&self.recall_all, ki),
                self.mean(&self.ndcg, ki)
            );
        }
        out.push_str(&format!("| {}", self.n));
        out
    }
}

// ---------------------------------------------------------------------------
// one question
// ---------------------------------------------------------------------------

struct QuestionOutcome {
    /// (mode, level) → metrics; levels: "sessione", "turno".
    metrics: HashMap<(&'static str, &'static str), Metrics>,
}

fn eval_question(
    q: &Question,
    hits: &[Hit],
    by_id: &HashMap<Ulid, usize>,
    rounds: &[Round],
) -> HashMap<(&'static str, &'static str), Metrics> {
    let relevant_rounds = rounds.iter().filter(|r| r.relevant).count();
    let answer_sessions: HashSet<&str> = q.answer_session_ids.iter().map(String::as_str).collect();

    let mut out: HashMap<(&'static str, &'static str), Metrics> = HashMap::new();
    let mut turn = Metrics::default();
    let mut session = Metrics::default();

    // turn level: the ranked rounds as they come back (note ↔ round 1:1)
    let ranked_turns: Vec<bool> = hits
        .iter()
        .filter_map(|h| by_id.get(&h.note.id))
        .map(|&i| rounds[i].relevant)
        .collect();
    turn.add_ranking(&ranked_turns, relevant_rounds);

    // session level: distinct sessions in rank order, until k of them
    let mut order: Vec<&str> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    for h in hits {
        if let Some(&i) = by_id.get(&h.note.id) {
            let s = rounds[i].session.as_str();
            if seen.insert(s) {
                order.push(s);
            }
        }
    }
    let ranked_sessions: Vec<bool> = order.iter().map(|s| answer_sessions.contains(s)).collect();
    session.add_ranking(&ranked_sessions, answer_sessions.len());

    out.insert(("turno", "x"), turn);
    out.insert(("sessione", "x"), session);
    out
}

fn run_question(
    q: &Question,
    root: &Path,
    mode: &str,
    embedder: &Option<Arc<CachedBuiltin>>,
) -> QuestionOutcome {
    let rounds = rounds_of(q);
    let name = format!("lme-{}", q.question_id);
    let db = root.join("cells").join(format!("{name}.sqlite"));
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| die(format!("cannot create {}: {e}", parent.display())));
    }
    let cell = Cell::create(&db, &name, "longmemeval")
        .unwrap_or_else(|e| die(format!("{}: cell create: {e}", q.question_id)));
    let mut by_id: HashMap<Ulid, usize> = HashMap::with_capacity(rounds.len());
    for (i, round) in rounds.iter().enumerate() {
        let id = Ulid::new();
        let note = Note {
            id,
            note_type: NoteType::Note,
            title: None,
            body: round.body.clone(),
            tags: vec![],
            scope: None,
            lang: Some(Lang::En),
            source: "longmemeval".into(),
            confidence: Confidence::Stated,
            status: Status::Active,
            valid_from: None,
            valid_to: None,
            supersedes: vec![],
            created_at: At(round.at),
            updated_at: At(round.at),
            extensions: vec![
                ("x-lme-session".into(), YamlValue::Str(round.session.clone())),
                ("x-lme-round".into(), YamlValue::Int(i as i64)),
            ],
        };
        cell.put_note(&note, "longmemeval")
            .unwrap_or_else(|e| die(format!("{}: put: {e}", q.question_id)));
        by_id.insert(id, i);
    }

    let now = parse_when(&q.question_date);
    let mut metrics: HashMap<(&'static str, &'static str), Metrics> = HashMap::new();
    let mut absorb = |mode: &'static str, m: HashMap<(&'static str, &'static str), Metrics>| {
        for (level, _x) in [("sessione", "x"), ("turno", "x")] {
            if let Some(v) = m.get(&(level, "x")) {
                metrics.insert((mode, level), *v);
            }
        }
    };

    if mode == "text" || mode == "both" {
        let mut query = Query::text(&q.question);
        query.k = TOP_K;
        query.graph_hops = 0;
        query.lang = Some(Lang::En);
        let hits = cell
            .search_with_now(&query, now)
            .unwrap_or_else(|e| die(format!("{}: search: {e}", q.question_id)));
        absorb("text", eval_question(q, &hits, &by_id, &rounds));
    }
    if mode == "hybrid" || mode == "both" {
        let Some(embedder) = embedder else {
            die("hybrid needs the builtin model");
        };
        cell.add_model(embedder.model_id(), embedder.dim())
            .unwrap_or_else(|e| die(format!("{}: add model: {e}", q.question_id)));
        let report = cell
            .embed_pending(embedder.as_ref(), rounds.len().saturating_add(1))
            .unwrap_or_else(|e| die(format!("{}: embed: {e}", q.question_id)));
        if report.remaining != 0 {
            die(format!(
                "{}: {} rounds left unembedded",
                q.question_id, report.remaining
            ));
        }
        let sem = Semantic::new(Arc::clone(embedder) as Arc<dyn Embedder>);
        let mut query = Query::text(&q.question);
        query.k = TOP_K;
        query.lang = Some(Lang::En);
        let hits = cell
            .search_semantic_with_now(&query, now, &sem)
            .unwrap_or_else(|e| die(format!("{}: hybrid search: {e}", q.question_id)));
        absorb("hybrid", eval_question(q, &hits, &by_id, &rounds));
    }

    drop(cell);
    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(db.with_extension("sqlite-wal"));
    let _ = std::fs::remove_file(db.with_extension("sqlite-shm"));
    QuestionOutcome { metrics }
}

// ---------------------------------------------------------------------------
// main: arguments, subset, aggregation, report
// ---------------------------------------------------------------------------

struct Args {
    mode: String,
    subset: usize,
    data: Option<PathBuf>,
    out: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut args = Args {
        mode: "both".into(),
        subset: 0,
        data: None,
        out: None,
    };
    let mut it = std::env::args().skip(2);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--mode" => {
                args.mode = it
                    .next()
                    .unwrap_or_else(|| die("--mode wants text|hybrid|both"));
            }
            "--subset" => {
                args.subset = it
                    .next()
                    .unwrap_or_else(|| die("--subset wants a number"))
                    .parse()
                    .unwrap_or_else(|e| die(format!("--subset: {e}")));
            }
            "--data" => {
                args.data = Some(PathBuf::from(
                    it.next().unwrap_or_else(|| die("--data wants a path")),
                ));
            }
            "--out" => {
                args.out = Some(PathBuf::from(
                    it.next().unwrap_or_else(|| die("--out wants a path")),
                ));
            }
            other => die(format!("unknown argument `{other}`")),
        }
    }
    if !matches!(args.mode.as_str(), "text" | "hybrid" | "both") {
        die("--mode wants text|hybrid|both");
    }
    args
}

/// A tiny deterministic LCG (the bench's): same picks on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// N questions, seed 42, proportional per `question_type` (largest
/// remainder), the ids printed so the subset is reproducible by hand.
fn pick_subset(questions: &[Question], n: usize) -> Vec<usize> {
    let mut by_type: HashMap<&str, Vec<usize>> = HashMap::new();
    for (i, q) in questions.iter().enumerate() {
        by_type.entry(q.question_type.as_str()).or_default().push(i);
    }
    let total = questions.len();
    let mut exact: Vec<(usize, &str, f64)> = Vec::new(); // (take, type, remainder)
    let mut assigned = 0_usize;
    for (t, ids) in &by_type {
        let share = ids.len() as f64 * n as f64 / total as f64;
        let take = share.floor() as usize;
        assigned += take;
        exact.push((take, t, share - take as f64));
    }
    exact.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    let mut left = n - assigned;
    let mut ti = 0;
    while left > 0 {
        if let Some(slot) = exact.get_mut(ti % exact.len()) {
            slot.0 += 1;
        }
        left -= 1;
        ti += 1;
    }
    let mut picked = Vec::with_capacity(n);
    for (take, t, _) in &exact {
        let mut ids = by_type[*t].clone();
        let mut rng = Rng(SUBSET_SEED ^ u64::from(t.len() << 8) ^ t.bytes().map(u64::from).sum::<u64>());
        for i in (1..ids.len()).rev() {
            let j = rng.below(i + 1);
            ids.swap(i, j);
        }
        picked.extend(ids.into_iter().take(*take));
    }
    picked.sort_unstable();
    picked
}

fn rss_mib() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Some(kb) = rest.split_whitespace().next() {
                    if let Ok(kb) = kb.parse::<u64>() {
                        return kb / 1024;
                    }
                }
            }
        }
        0
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output();
        match out {
            Ok(o) => String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u64>()
                .map(|kb| kb / 1024)
                .unwrap_or(0),
            Err(_) => 0,
        }
    }
}

fn main() {
    let started = Instant::now();
    let args = parse_args();

    let data = args.data.clone().unwrap_or_else(|| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        PathBuf::from(home)
            .join(".cache")
            .join("engramma")
            .join("bench")
            .join("longmemeval_s_cleaned.json")
    });
    if !data.is_file() {
        die(format!(
            "dataset missing: {} (expected sha256 {EXPECTED_SHA256}; it never enters the repo)",
            data.display()
        ));
    }
    let bytes = std::fs::read(&data).unwrap_or_else(|e| die(format!("{}: {e}", data.display())));
    let digest = ring::digest::digest(&ring::digest::SHA256, &bytes);
    let hex: String = digest
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if hex != EXPECTED_SHA256 {
        die(format!(
            "sha256 mismatch on {}: expected {EXPECTED_SHA256}, found {hex}",
            data.display()
        ));
    }
    let questions: Vec<Question> = serde_json::from_slice(&bytes)
        .unwrap_or_else(|e| die(format!("{}: {e}", data.display())));
    drop(bytes);
    let total = questions.len();
    let questions: Vec<Question> = questions
        .into_iter()
        .filter(|q| !q.question_id.ends_with("_abs"))
        .collect();
    eprintln!(
        "{} questions ({} of {total}; the _abs ones are skipped)",
        questions.len(),
        questions.len()
    );

    let chosen: Vec<usize> = if args.subset > 0 && args.subset < questions.len() {
        let picked = pick_subset(&questions, args.subset);
        let ids: Vec<&str> = picked
            .iter()
            .map(|&i| questions[i].question_id.as_str())
            .collect();
        eprintln!("subset of {}: {}", args.subset, ids.join(" "));
        picked
    } else {
        (0..questions.len()).collect()
    };

    // The cells live in a temp directory, gone at the end of the process.
    let root = tempfile::tempdir().unwrap_or_else(|e| die(format!("tempdir: {e}")));

    let embedder: Option<Arc<CachedBuiltin>> = if args.mode != "text" {
        let model_root = engramma_embed::spec::model_root();
        let builtin = engramma_embed::Builtin::load_named(
            &model_root,
            "granite-embedding-97m-multilingual-r2",
        )
        .unwrap_or_else(|e| die(format!("builtin model: {e}")));
        let safe: String = builtin
            .model_id()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let cache_dir = model_root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("bench");
        let cache = Arc::new(VectorCache::open(
            &cache_dir.join(format!("vec-cache-{safe}.bin")),
            builtin.dim(),
        ));
        Some(Arc::new(CachedBuiltin { inner: builtin, cache }))
    } else {
        None
    };

    // (mode, level, type) → metrics
    let mut table: HashMap<(&str, &str, &str), Metrics> = HashMap::new();
    let mut max_rss = 0_u64;
    for (n, &qi) in chosen.iter().enumerate() {
        let q = &questions[qi];
        let outcome = run_question(q, root.path(), &args.mode, &embedder);
        for ((mode, level), m) in outcome.metrics {
            let slot = table.entry((mode, level, &q.question_type)).or_default();
            merge(slot, &m);
            let total_slot = table.entry((mode, level, "TOTALE")).or_default();
            merge(total_slot, &m);
        }
        max_rss = max_rss.max(rss_mib());
        if (n + 1) % 25 == 0 {
            eprintln!(
                "{}/{} questions, {:.0} s, rss max {} MB",
                n + 1,
                chosen.len(),
                started.elapsed().as_secs_f64(),
                max_rss
            );
        }
    }

    let mut report = String::new();
    report.push_str("# LongMemEval-S (cleaned) — engramma v0.4.4 (P3)\n\n");
    report.push_str(&format!(
        "- dati: `{}` (sha256 verificato)\n- domande: {} (saltate le `_abs`){}\n- top {} round per domanda, livelli sessione e turno, k = 5 e 10\n",
        data.display(),
        chosen.len(),
        if args.subset > 0 { format!(", sottoinsieme fisso di {}", args.subset) } else { ", tutte".into() },
        TOP_K
    ));
    report.push_str(&format!(
        "- durata: {:.0} s; RSS massima: {} MB\n",
        started.elapsed().as_secs_f64(),
        max_rss
    ));
    if let Some(e) = &embedder {
        report.push_str(&format!(
            "- cache vettori: {} record nuovi, {} riusi (i riusi coprono i round condivisi tra domande)\n",
            e.cache.misses.load(Ordering::Relaxed),
            e.cache.hits.load(Ordering::Relaxed)
        ));
        report.push_str(&format!(
            "- testo incorporato in questa corsa: {} caratteri\n",
            e.cache.embedded_chars.load(Ordering::Relaxed)
        ));
    }
    report.push_str("\n| modalità | livello | tipo | any@5 | all@5 | ndcg@5 | any@10 | all@10 | ndcg@10 | n |\n");
    report.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
    let mut modes: Vec<&str> = table.keys().map(|(m, _, _)| *m).collect();
    modes.dedup();
    let mut rows: Vec<String> = Vec::new();
    for mode in &modes {
        for level in ["sessione", "turno"] {
            for qtype in ["TOTALE", "knowledge-update", "multi-session", "single-session-assistant", "single-session-preference", "single-session-user", "temporal-reasoning"] {
                if let Some(m) = table.get(&(mode, level, qtype)) {
                    rows.push(format!("| {mode} | {level} | {qtype} {}", m.cells()));
                }
            }
        }
    }
    report.push_str(&rows.join("\n"));
    report.push('\n');
    println!("{report}");
    if let Some(out) = &args.out {
        std::fs::write(out, report.as_bytes())
            .unwrap_or_else(|e| die(format!("{}: {e}", out.display())));
        eprintln!("scritto {}", out.display());
    }
}

fn merge(dst: &mut Metrics, src: &Metrics) {
    for ki in 0..KS.len() {
        dst.recall_any[ki] += src.recall_any[ki];
        dst.recall_all[ki] += src.recall_all[ki];
        dst.ndcg[ki] += src.ndcg[ki];
    }
    dst.n += src.n;
    dst.n_rel += src.n_rel;
}
