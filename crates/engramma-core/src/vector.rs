// SPDX-License-Identifier: Apache-2.0

//! The vector index (DESIGN §6.2): an int8 flat scan with f32 recomputation
//! of the top candidates. sqlite-vec was measured slower (28 ms vs 0.7 ms on
//! 100k×384) and its stable line has no ANN; when DiskANN lands behind this
//! same trait, or cells grow past a million notes (HNSW), only the
//! implementation changes.

use std::collections::HashMap;

/// One candidate from an index scan.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexHit {
    /// Note the vector belongs to.
    pub note_id: String,
    /// Chunk index inside the note.
    pub chunk: i64,
    /// Approximate cosine from the int8 scan — the caller recomputes the
    /// exact f32 dot for the candidates that survive its filters.
    pub score: f32,
}

/// Behaviour of a vector index (§6.2): search, incremental update, rebuild.
pub trait VectorIndex {
    /// The `m` best candidates for `query` by approximate cosine.
    fn search(&self, query: &[f32], m: usize) -> Vec<IndexHit>;

    /// Replace the vectors of one note (`chunks` is (chunk index, vector)).
    fn upsert(&mut self, note_id: &str, chunks: &[(i64, Vec<f32>)]);

    /// Forget a note entirely.
    fn remove(&mut self, note_id: &str);

    /// Build from scratch from stored rows.
    fn rebuild(&mut self, rows: Vec<IndexRow>);

    /// Live vectors held.
    fn len(&self) -> usize;

    /// No live vectors.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One stored vector, as loaded for a (re)build.
#[derive(Debug, Clone)]
pub struct IndexRow {
    /// Note id.
    pub note_id: String,
    /// Chunk index.
    pub chunk: i64,
    /// The vector (L2-normalised).
    pub vec: Vec<f32>,
}

/// Flat int8 index with symmetric per-vector quantisation (§6.2, measured
/// 2026-11-05): quantise the query the same way, integer dot products over
/// the whole matrix, top `m` candidates. The exact f32 recomputation of the
/// candidates happens in the caller, against `note_vectors`.
pub struct FlatI8 {
    dim: usize,
    /// Row-major quantised vectors, `rows.len()` rows of `dim` i8s.
    quant: Vec<i8>,
    /// Per-row reciprocal of the quantisation scale: one multiply per row
    /// turns the integer dot into the approximate cosine (v0.2.1 — the
    /// per-row division kept the scan off the SIMD path).
    inv_scales: Vec<f32>,
    rows: Vec<Row>,
    /// note_id -> row slots.
    by_note: HashMap<String, Vec<usize>>,
    live: usize,
}

struct Row {
    note_id: String,
    chunk: i64,
    alive: bool,
}

/// Quantise one vector: returns the i8 codes and the scale.
fn quantise(v: &[f32]) -> (Vec<i8>, f32) {
    let max_abs = v.iter().fold(0.0_f32, |a, x| a.max(x.abs()));
    if max_abs <= 0.0 {
        return (vec![0; v.len()], 1.0);
    }
    let scale = 127.0 / max_abs;
    let codes = v
        .iter()
        .map(|x| {
            let q = (x * scale).round();
            q.clamp(-127.0, 127.0) as i8
        })
        .collect();
    (codes, scale)
}

impl FlatI8 {
    /// An empty index for `dim`-dimensional vectors.
    #[must_use]
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            quant: Vec::new(),
            inv_scales: Vec::new(),
            rows: Vec::new(),
            by_note: HashMap::new(),
            live: 0,
        }
    }

    /// The dimension this index was built for.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.dim
    }

    fn push_row(&mut self, note_id: &str, chunk: i64, vec: &[f32]) {
        if vec.len() != self.dim {
            return;
        }
        let (codes, scale) = quantise(vec);
        let slot = self.rows.len();
        self.quant.extend_from_slice(&codes);
        self.inv_scales.push(1.0 / scale);
        self.rows.push(Row {
            note_id: note_id.to_string(),
            chunk,
            alive: true,
        });
        self.by_note
            .entry(note_id.to_string())
            .or_default()
            .push(slot);
        self.live += 1;
    }

    /// Exact f32 dot product with the stored vector of one row (used by
    /// callers that already hold the f32 vectors; kept here so the maths
    /// lives in one place).
    #[must_use]
    pub fn row_dot(&self, row: usize, v: &[f32]) -> f32 {
        let start = row * self.dim;
        let end = start + self.dim;
        let codes = &self.quant[start..end];
        let inv = self.inv_scales[row];
        // dequantise on the fly: code * (1/s)
        let acc: f32 = codes
            .iter()
            .zip(v.iter())
            .map(|(c, x)| f32::from(*c) * x * inv)
            .sum();
        acc
    }

    /// Integer dot of the query codes against every live row, top `m`.
    /// v0.2.1 rewrite: `chunks_exact` over rows and over the 8-wide lanes
    /// (no per-element indexing, no bounds checks for LLVM to defeat), one
    /// multiply per row through the precomputed reciprocal scale, scores in
    /// a Vec finished by `select_nth_unstable_by` (the running sorted
    /// insertion cost O(m) per row). 100k×384: 8.6 ms → 0.7 ms.
    fn top_int(&self, qcodes: &[i8], qscale: f32, m: usize) -> Vec<IndexHit> {
        let dim = self.dim;
        let qscale_inv = 1.0 / qscale;
        let mut scored: Vec<(f32, u32)> = Vec::with_capacity(self.live);
        for (row, (codes, inv)) in self
            .quant
            .chunks_exact(dim)
            .zip(self.inv_scales.iter())
            .enumerate()
        {
            if !self.rows[row].alive {
                continue;
            }
            // 8 independent i32 accumulators, chunks_exact(8): LLVM
            // vectorises the loop (measured ~20x over the naive sum).
            let mut acc = [0_i32; 8];
            for (a8, q8) in codes.chunks_exact(8).zip(qcodes.chunks_exact(8)) {
                for j in 0..8 {
                    acc[j] += i32::from(a8[j]) * i32::from(q8[j]);
                }
            }
            let mut rest = 0_i32;
            let rem = dim % 8;
            if rem != 0 {
                let a_tail = &codes[dim - rem..];
                let q_tail = &qcodes[dim - rem..];
                for j in 0..rem {
                    rest += i32::from(a_tail[j]) * i32::from(q_tail[j]);
                }
            }
            let dot = acc.iter().sum::<i32>() + rest;
            let score = dot as f32 * (inv * qscale_inv);
            scored.push((score, u32::try_from(row).unwrap_or(u32::MAX)));
        }
        let m = m.min(scored.len());
        if m == 0 {
            return Vec::new();
        }
        scored.select_nth_unstable_by(m - 1, |a, b| {
            b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
        });
        scored.truncate(m);
        // deterministic order: score descending, then row ascending (the
        // v0.2 running insertion kept equal scores in row order)
        scored.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
        scored
            .into_iter()
            .map(|(score, row)| {
                let row = row as usize;
                IndexHit {
                    note_id: self.rows[row].note_id.clone(),
                    chunk: self.rows[row].chunk,
                    score,
                }
            })
            .collect()
    }
}

impl VectorIndex for FlatI8 {
    fn search(&self, query: &[f32], m: usize) -> Vec<IndexHit> {
        if m == 0 || self.live == 0 || query.len() != self.dim {
            return Vec::new();
        }
        let (qcodes, qscale) = quantise(query);
        self.top_int(&qcodes, qscale, m)
    }

    fn upsert(&mut self, note_id: &str, chunks: &[(i64, Vec<f32>)]) {
        self.remove(note_id);
        for (chunk, vec) in chunks {
            self.push_row(note_id, *chunk, vec);
        }
    }

    fn remove(&mut self, note_id: &str) {
        if let Some(slots) = self.by_note.remove(note_id) {
            for slot in slots {
                if self.rows[slot].alive {
                    self.rows[slot].alive = false;
                    self.live -= 1;
                }
            }
        }
    }

    fn rebuild(&mut self, rows: Vec<IndexRow>) {
        self.quant.clear();
        self.inv_scales.clear();
        self.rows.clear();
        self.by_note.clear();
        self.live = 0;
        for r in rows {
            self.push_row(&r.note_id, r.chunk, &r.vec);
        }
    }

    fn len(&self) -> usize {
        self.live
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic xorshift, as everywhere else in the crate.
    struct Rng(u64);
    impl Rng {
        fn next_f32(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            ((self.0 >> 40) as f32 / 16_777_216.0) - 1.0
        }
    }

    fn random_unit(rng: &mut Rng, dim: usize) -> Vec<f32> {
        let mut v: Vec<f32> = (0..dim).map(|_| rng.next_f32()).collect();
        crate::embed::l2_normalize(&mut v);
        v
    }

    /// Brute-force exact top-k (the reference implementation).
    fn exact_top(rows: &[IndexRow], q: &[f32], k: usize) -> Vec<(String, i64)> {
        let mut scored: Vec<(f32, &IndexRow)> = rows
            .iter()
            .map(|r| (crate::embed::dot_f32(&r.vec, q), r))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored
            .into_iter()
            .take(k)
            .map(|(_, r)| (r.note_id.clone(), r.chunk))
            .collect()
    }

    fn recall_at_10(dim: usize, n: usize, seed: u64) -> f32 {
        let mut rng = Rng(seed | 1);
        let rows: Vec<IndexRow> = (0..n)
            .map(|i| IndexRow {
                note_id: format!("n{i:06}"),
                chunk: 0,
                vec: random_unit(&mut rng, dim),
            })
            .collect();
        let mut index = FlatI8::new(dim);
        index.rebuild(rows.clone());
        let mut hits = 0_usize;
        let queries = 50_usize;
        for _qi in 0..queries {
            let q = random_unit(&mut rng, dim);
            let want = exact_top(&rows, &q, 10);
            let got: Vec<(String, i64)> = index
                .search(&q, 200)
                .into_iter()
                .map(|h| (h.note_id, h.chunk))
                .collect();
            let overlap = want.iter().filter(|w| got.contains(w)).count();
            hits += overlap;
        }
        hits as f32 / (queries as f32 * 10.0)
    }

    #[test]
    fn recall_on_random_vectors_meets_the_bar() {
        let r384 = recall_at_10(384, 10_000, 0x2026_1105);
        assert!(r384 >= 0.98, "recall@10 on 10k x 384: {r384:.4}");
        let r768 = recall_at_10(768, 5_000, 0x2026_1106);
        assert!(r768 >= 0.98, "recall@10 on 5k x 768: {r768:.4}");
    }

    #[test]
    fn upsert_remove_and_incremental_equals_rebuilt() {
        let mut rng = Rng(0xABCD_1234);
        let dim = 64;
        let mut rows: Vec<IndexRow> = (0..200)
            .map(|i| IndexRow {
                note_id: format!("n{i:03}"),
                chunk: 0,
                vec: random_unit(&mut rng, dim),
            })
            .collect();
        let mut incremental = FlatI8::new(dim);
        incremental.rebuild(rows.clone());
        // random writes: edits (upsert), removals, additions
        for step in 0..120 {
            let victim = (rng.next_f32().abs() * 200.0) as usize % rows.len().max(1);
            match step % 3 {
                0 => {
                    // edit
                    let v = random_unit(&mut rng, dim);
                    rows[victim].vec = v.clone();
                    incremental.upsert(&rows[victim].note_id.clone(), &[(0, v)]);
                }
                1 if rows.len() > 50 => {
                    // remove
                    let r = rows.remove(victim);
                    incremental.remove(&r.note_id);
                }
                _ => {
                    // add
                    let id = format!("new{step:03}");
                    let v = random_unit(&mut rng, dim);
                    rows.push(IndexRow {
                        note_id: id.clone(),
                        chunk: 0,
                        vec: v.clone(),
                    });
                    incremental.upsert(&id, &[(0, v)]);
                }
            }
        }
        let mut rebuilt = FlatI8::new(dim);
        rebuilt.rebuild(rows.clone());
        assert_eq!(incremental.len(), rebuilt.len());
        for qi in 0..30 {
            let q = random_unit(&mut rng, dim);
            let a: Vec<(String, i64)> = incremental
                .search(&q, 10)
                .into_iter()
                .map(|h| (h.note_id, h.chunk))
                .collect();
            let b: Vec<(String, i64)> = rebuilt
                .search(&q, 10)
                .into_iter()
                .map(|h| (h.note_id, h.chunk))
                .collect();
            assert_eq!(a, b, "query {qi}: incremental index diverged");
        }
    }

    #[test]
    fn empty_and_wrong_dim_queries_are_quiet() {
        let mut index = FlatI8::new(8);
        assert!(index.is_empty());
        assert!(index.search(&[0.0; 8], 10).is_empty());
        index.rebuild(vec![IndexRow {
            note_id: "n".into(),
            chunk: 0,
            vec: vec![1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        }]);
        assert!(
            index
                .search(&[1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], 0)
                .is_empty()
        );
        // wrong dim: no panic, no results
        assert!(index.search(&[1.0, 0.0], 10).is_empty());
    }

    #[test]
    fn quantise_round_trips_within_tolerance() {
        let v = vec![0.3, -0.8, 0.5, 0.0, -0.1, 0.9, -0.4, 0.2];
        let (codes, scale) = quantise(&v);
        let back: Vec<f32> = codes.iter().map(|c| f32::from(*c) / scale).collect();
        for (a, b) in v.iter().zip(back.iter()) {
            assert!((a - b).abs() < 0.01, "{v:?} -> {back:?}");
        }
    }
}
