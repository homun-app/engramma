// SPDX-License-Identifier: Apache-2.0

//! The in-memory graph index (DESIGN §6.3.3, v0.3): notes and entities as
//! integer nodes, visible edges as an undirected CSR adjacency, so the
//! personalised PageRank walk and `neighbors`/`path` never read SQLite per
//! level. Derived like `FlatI8`: built lazily on the first use, patched by
//! this cell's own writes (a small overlay over the frozen CSR), rebuilt when
//! another process writes (`PRAGMA data_version`) or the overlay grows past a
//! fraction of the graph.
//!
//! Two kinds of adjacency live here. **Real** edges are the `edges` rows
//! (`note:<id>` ↔ entity mentions, entity ↔ entity relations). **Declared**
//! adjacencies are the bipartite projection the walk needs: the note that
//! declares an edge is linked to the edge's endpoints, because a relation
//! row (`Giulia works_at Studio Rossi`) does not otherwise touch the note
//! that states it — the v0.2 walk discovered that note through `note_id`,
//! the prototype linked it structurally. `neighbors` and `path` see real
//! edges only; the push sees both.
//!
//! The push is **weighted**: a typed relation is an assertion (weight 1), a
//! `mentions` link is navigation (weight [`MENTION_WEIGHT`]). On an unweighted
//! walk the ~40 filler notes mentioning the subject drown the one `partner_of`
//! edge the multi-hop questions are about — measured on the 10k benchmark,
//! where the graph list amplified the subject's own neighbourhood instead of
//! extending it (HippoRAG 2 walks the triple graph, not document mentions).
//!
//! The walk itself is an Andersen–Chung–Lang forward push: a node `u` is
//! expanded only while its residual mass clears `ε·deg(u)`, which is why hubs
//! with tens of thousands of edges cost ~nothing (measured on the v0.3
//! prototype: ~157 pushed edges per question at ε = 10⁻³ on a 100k-note
//! bipartite graph whose largest hub has 16 741 edges).

use std::collections::HashMap;

use rusqlite::Connection;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::error::SearchError;

/// Sentinel for "no declaring note" in edge metadata.
const NO_NOTE: u32 = u32::MAX;

/// Walk weight of a `mentions` edge: navigation, not assertion. Typed
/// relations and their declared adjacencies weigh 1.
const MENTION_WEIGHT: f64 = 0.25;

/// Default capacity of the query-embedding LRU (prompt v0.3: configurable,
/// 4 096 default — the v0.2.1 256 made "warm" benchmarks re-embed).
pub const QUERY_CACHE_CAPACITY: usize = 4_096;

/// The name key of [`GraphIndex::names`] (v0.4, P1.4): the §6.1
/// normalization without stemming — lowercase, Latin diacritics folded,
/// tokens (non-alphanumeric splits) joined by one space. Entity names are
/// proper nouns: their stem would lose the very thing being matched.
fn normalize_name(name: &str) -> String {
    let folded = crate::text::strip_diacritics(&name.to_lowercase());
    let tokens: Vec<&str> = folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    tokens.join(" ")
}

/// The `aliases` of a note's canonical frontmatter JSON, verbatim.
fn aliases_of(frontmatter_json: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(frontmatter_json) else {
        return Vec::new();
    };
    v.get("x")
        .and_then(|x| x.as_array())
        .map(|exts| {
            exts.iter()
                .filter_map(|ext| {
                    let mut it = ext.as_array()?.iter();
                    let key = it.next()?.as_str()?;
                    if key != "aliases" {
                        return None;
                    }
                    it.next()?.as_array().map(|vals| {
                        vals.iter()
                            .filter_map(|a| a.as_str().map(str::to_string))
                            .collect::<Vec<String>>()
                    })
                })
                .flatten()
                .collect()
        })
        .unwrap_or_default()
}

/// u32 node/relation ids to vector indexes (u32 always fits usize here).
fn ix(n: u32) -> usize {
    n as usize
}

/// Walk weight of a relation by name: `mentions` links navigate, typed
/// relations assert.
fn walk_weight(rel: &str) -> f64 {
    if rel == "mentions" {
        MENTION_WEIGHT
    } else {
        1.0
    }
}

/// Visibility class of an edge, mirroring `edges.status` at (re)build time.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EdgeVisibility {
    /// Declared by an active (or superseded, or manual) edge: always walkable.
    Visible,
    /// Declared by a quarantined note: walkable only with `include_quarantined`.
    Quarantined,
    /// Declared by a deleted note: never walkable.
    Deleted,
}

impl EdgeVisibility {
    fn from_status(s: &str) -> Self {
        match s {
            "quarantined" => EdgeVisibility::Quarantined,
            "deleted" => EdgeVisibility::Deleted,
            _ => EdgeVisibility::Visible,
        }
    }

    fn walkable(self, include_quarantined: bool) -> bool {
        match self {
            EdgeVisibility::Visible => true,
            EdgeVisibility::Quarantined => include_quarantined,
            EdgeVisibility::Deleted => false,
        }
    }
}

/// One neighbour in a traversal.
pub(crate) struct Step<'a> {
    /// The node on the other side.
    pub node: u32,
    /// Relation name.
    pub rel: &'a str,
    /// Declaring note node, when present (real edges only).
    pub note: Option<u32>,
    /// `->` when the edge points away from the walked node, `<-` otherwise.
    pub forward: bool,
    /// Validity window id (see [`GraphIndex::valid_at`] and
    /// [`GraphIndex::valid_strings`]).
    pub(crate) validity: u16,
    /// Walk weight (relation 1, mention [`MENTION_WEIGHT`]).
    pub(crate) weight: f64,
}

/// Where the edges a note declares live in the index. Base slots come in
/// pairs (one per direction); overlay edges are shared by both endpoints.
#[derive(Clone, Copy)]
enum Loc {
    Base(u32, u32),
    Overlay(u32),
}

/// An edge added after the CSR was frozen (real or declared).
struct OverlayEdge {
    from: u32,
    to: u32,
    rel: u32,
    validity: u16,
    status: EdgeVisibility,
    note: u32,
    synthetic: bool,
    weight: f64,
    alive: bool,
}

/// The CSR adjacency entry (one direction of one edge).
#[derive(Clone, Copy)]
struct AdjEntry {
    other: u32,
    rel: u32,
    validity: u16,
    status: EdgeVisibility,
    note: u32,
    /// True when this slot sits on the source side of the edge: walking it
    /// from the owning node goes `src -> dst`.
    src_side: bool,
    /// True for the bipartite projection (declared adjacencies), which only
    /// the push traverses.
    synthetic: bool,
    /// Walk weight.
    weight: f64,
}

/// Validity window in microseconds since the Unix epoch: `[from, to)` with
/// `i64::MIN`/`i64::MAX` for the open sides (the format is second-precision,
/// so microseconds lose nothing).
type Window = (i64, i64);

/// One raw edge as read from the table.
struct RawEdge {
    src: String,
    rel: String,
    dst: String,
    note_id: Option<String>,
    valid_from: Option<String>,
    valid_to: Option<String>,
    status: String,
}

/// An edge resolved to node ids, ready to place.
#[derive(Clone)]
struct Resolved {
    src: u32,
    dst: u32,
    rel: u32,
    validity: u16,
    status: EdgeVisibility,
    note: u32,
    synthetic: bool,
    weight: f64,
}

/// The graph index. Rebuild it with [`GraphIndex::build`]; never mutate the
/// CSR in place — own-process writes go through [`GraphIndex::patch_note`].
pub(crate) struct GraphIndex {
    /// Interned node keys: entity names lowercased, or `note:<ulid>`.
    keys: Vec<String>,
    /// Original-case entity name (or the `note:` key) for display.
    display: Vec<String>,
    node_map: HashMap<String, u32>,
    /// `Some(ulid)` for note nodes.
    node_note: Vec<Option<Ulid>>,
    /// v0.5.3 (P1): the status of the note a node stands for (the same
    /// tri-state the edges use; `Visible` for pure entity nodes). The push's
    /// OUTPUT list carries visible notes only — a hidden note may receive
    /// mass as an entity (other notes' walkable edges point at it) but it
    /// never enters the list, so it can neither take a slot nor move the
    /// max-normalisation of the visible entries.
    note_vis: Vec<EdgeVisibility>,
    /// CSR offsets, `keys.len() + 1` entries.
    offsets: Vec<u32>,
    /// CSR adjacency (both directions of every edge).
    adj: Vec<AdjEntry>,
    /// Note node -> where its declared edges live.
    edges_of_note: HashMap<u32, Vec<Loc>>,
    /// CSR slots removed by a patch (both directions marked).
    removed: std::collections::HashSet<u32>,
    /// Edges added by a patch, referenced from both endpoints.
    overlay: Vec<OverlayEdge>,
    overlay_by_node: HashMap<u32, Vec<u32>>,
    /// Live weighted degree (base minus removed plus overlay) — the push
    /// threshold is proportional to it and shares split by it.
    degree: Vec<f64>,
    /// Relation interning.
    rels: Vec<String>,
    rel_map: HashMap<String, u32>,
    /// Validity interning.
    windows: Vec<Window>,
    window_map: HashMap<Window, u16>,
    /// The original `valid_from`/`valid_to` strings per window (display).
    window_strings: Vec<(Option<String>, Option<String>)>,
    /// Live `edges`-table rows (declared adjacencies excluded).
    real_edges: usize,
    /// Live adjacencies of both kinds.
    live_edges: usize,
    /// v0.4 (P1.4): normalized name → node, for the question's entities.
    /// Entity names (any case) plus the `aliases` of entity-notes.
    names: HashMap<String, u32>,
    /// The aliases each entity-note gave, to drop them when it rewrites.
    aliases_of_note: HashMap<u32, Vec<String>>,
}

impl GraphIndex {
    /// Read every edge and build the CSR.
    ///
    /// # Errors
    /// [`SearchError::Db`] on SQL failure.
    pub fn build(conn: &Connection) -> Result<Self, SearchError> {
        let mut gi = GraphIndex {
            keys: Vec::new(),
            display: Vec::new(),
            node_map: HashMap::new(),
            node_note: Vec::new(),
            note_vis: Vec::new(),
            offsets: Vec::new(),
            adj: Vec::new(),
            edges_of_note: HashMap::new(),
            removed: std::collections::HashSet::new(),
            overlay: Vec::new(),
            overlay_by_node: HashMap::new(),
            degree: Vec::new(),
            rels: Vec::new(),
            rel_map: HashMap::new(),
            windows: Vec::new(),
            window_map: HashMap::new(),
            window_strings: Vec::new(),
            real_edges: 0,
            live_edges: 0,
            names: HashMap::new(),
            aliases_of_note: HashMap::new(),
        };
        let mut raw: Vec<RawEdge> = Vec::new();
        {
            let mut stmt = conn.prepare(
                "SELECT src, rel, dst, note_id, valid_from, valid_to, status FROM edges",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok(RawEdge {
                    src: r.get(0)?,
                    rel: r.get(1)?,
                    dst: r.get(2)?,
                    note_id: r.get(3)?,
                    valid_from: r.get(4)?,
                    valid_to: r.get(5)?,
                    status: r.get(6)?,
                })
            })?;
            for row in rows {
                raw.push(row?);
            }
        }
        // Pass 1: intern nodes and relation/validity ids, remember real pairs.
        let mut real: Vec<Resolved> = Vec::with_capacity(raw.len());
        for e in &raw {
            let src = gi.intern_endpoint(&e.src);
            let dst = gi.intern_endpoint(&e.dst);
            let note = e
                .note_id
                .as_deref()
                .map(|id| gi.intern_note_id(id))
                .unwrap_or(NO_NOTE);
            real.push(Resolved {
                src,
                dst,
                rel: gi.intern_rel(&e.rel),
                validity: gi.intern_window(&e.valid_from, &e.valid_to),
                status: EdgeVisibility::from_status(&e.status),
                note,
                synthetic: false,
                weight: walk_weight(&e.rel),
            });
        }
        // Pass 2: the declared projection — the note that states a relation
        // (an entity↔entity edge) links to the relation's endpoints. A note
        // that is already an endpoint (a mention) projects nothing: the first
        // version projected every edge, which duplicated each mention at full
        // weight and drowned the assertions (measured on the 10k multi-hop
        // diagnostics). Parallel adjacencies remain legitimate: a note
        // mentioning Fabio AND declaring `friend_of Fabio` carries both the
        // 0.25 navigation link and the 1.0 assertion.
        let mut all: Vec<Resolved> = real.clone();
        for e in &real {
            if e.note == NO_NOTE || e.note == e.src || e.note == e.dst {
                continue;
            }
            for endpoint in [e.src, e.dst] {
                all.push(Resolved {
                    src: e.note,
                    dst: endpoint,
                    rel: e.rel,
                    validity: e.validity,
                    status: e.status,
                    note: NO_NOTE,
                    synthetic: true,
                    weight: e.weight,
                });
            }
        }
        gi.real_edges = raw.len();
        gi.place(all);
        gi.index_names(conn)?;
        gi.refresh_note_vis(conn)?;
        Ok(gi)
    }

    /// v0.5.3 (P1): read the note statuses and file them under their
    /// nodes. v0.6.8 (H1): the bin's one predicate rides along — a copy
    /// that follows a lapide walks like a deleted note (its edges stay
    /// for the reopen, the walks do not see them).
    ///
    /// # Errors
    /// [`SearchError::Db`] on SQL failure.
    fn refresh_note_vis(&mut self, conn: &Connection) -> Result<(), SearchError> {
        let mut stmt = conn.prepare("SELECT id, status FROM notes")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut vis: HashMap<String, EdgeVisibility> = HashMap::new();
        for row in rows {
            let (id, status) = row?;
            vis.insert(id, EdgeVisibility::from_status(&status));
        }
        drop(stmt);
        let in_bin = crate::cell::bin::bin_note_ids(conn)
            .map_err(|e| SearchError::Db(e.to_string()))?;
        for id in in_bin {
            if let Some(v) = vis.get_mut(&id) {
                *v = EdgeVisibility::from_status("deleted");
            }
        }
        for node in 0..self.node_note.len() {
            if let Some(id) = self.node_note[node].as_ref() {
                let v = vis
                    .get(&id.to_string())
                    .copied()
                    .unwrap_or(EdgeVisibility::Visible);
                self.note_vis[node] = v;
            }
        }
        Ok(())
    }

    /// v0.5.3 (P1): may the note this node stands for appear in a walk's
    /// OUTPUT? (Entity nodes — `node_note` None — always may: they are not
    /// notes, and the walk through them is carried by other notes' edges.)
    fn note_visible(&self, node: u32, include_quarantined: bool) -> bool {
        if self.node_note.get(ix(node)).copied().flatten().is_none() {
            return true;
        }
        self.note_vis
            .get(ix(node))
            .copied()
            .unwrap_or(EdgeVisibility::Visible)
            .walkable(include_quarantined)
    }

    /// v0.5.3 (P1): does this node have at least one WALKABLE edge — an
    /// entity the visible graph actually speaks about? The question-entity
    /// gate seeds only such entities: an entity only hidden notes ever
    /// declared must not open the walk (its shares would dilute the real
    /// seeds, and the walk itself would be an oracle).
    pub(crate) fn has_walkable_step(
        &self,
        node: u32,
        as_of: Option<OffsetDateTime>,
        include_quarantined: bool,
    ) -> bool {
        self.steps(node, as_of, include_quarantined, &[], false)
            .next()
            .is_some()
    }

    /// Fill [`GraphIndex::names`] (v0.4, P1.4): every entity name, plus the
    /// `aliases` of the notes that ARE entities. The key is the index-side
    /// normalization of §6.1 — lowercase, Latin diacritics folded, tokens
    /// joined by one space — so «Anna Ròssi» and «anna rossi» are one.
    ///
    /// # Errors
    /// [`SearchError::Db`] on SQL failure.
    fn index_names(&mut self, conn: &Connection) -> Result<(), SearchError> {
        let mut entity_of_note: HashMap<String, String> = HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT name, note_id FROM entities")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))?;
            for row in rows {
                let (name, note_id) = row?;
                let node = self.intern_entity(&name, &name);
                self.names.insert(normalize_name(&name), node);
                if let Some(nid) = note_id {
                    entity_of_note.insert(nid, name);
                }
            }
        }
        // the aliases of the entity-notes: `x: [["aliases", [...]]]` in the
        // note's canonical JSON
        let mut stmt = conn.prepare("SELECT id, frontmatter_json FROM notes")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, fm) = row?;
            if !entity_of_note.contains_key(&id) {
                continue;
            }
            let Some(node) = entity_of_note
                .get(&id)
                .and_then(|name| self.node_of_entity(name))
            else {
                continue;
            };
            let aliases = aliases_of(&fm);
            if aliases.is_empty() {
                continue;
            }
            for a in &aliases {
                self.names.insert(normalize_name(a), node);
            }
            self.aliases_of_note.insert(node, aliases);
        }
        Ok(())
    }

    /// Drop and re-read one note's names (v0.4, P1.4): its entity note's
    /// aliases may have changed with the write.
    ///
    /// # Errors
    /// [`SearchError::Db`] on SQL failure.
    fn patch_names(&mut self, conn: &Connection, id: &Ulid) -> Result<(), SearchError> {
        let key = format!("note:{id}");
        let Some(node) = self.node_map.get(&key).copied() else {
            return Ok(());
        };
        if let Some(old) = self.aliases_of_note.remove(&node) {
            for a in old {
                self.names.remove(&normalize_name(&a));
            }
        }
        let name: Option<String> = conn
            .query_row(
                "SELECT name FROM entities WHERE note_id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .ok();
        let Some(name) = name else {
            return Ok(());
        };
        // the fresh name joins the index (a renamed entity leaves the old
        // key pointing at the same node until the next full rebuild — a
        // harmless stale alias, declared)
        let named = self.intern_entity(&name, &name);
        self.names.insert(normalize_name(&name), named);
        let fm: Option<String> = conn
            .query_row(
                "SELECT frontmatter_json FROM notes WHERE id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .ok();
        let aliases = fm.as_deref().map(aliases_of).unwrap_or_default();
        if !aliases.is_empty() {
            for a in &aliases {
                self.names.insert(normalize_name(a), node);
            }
            self.aliases_of_note.insert(node, aliases);
        }
        Ok(())
    }

    /// The node an entity name (or alias) points to, by its normalized key.
    pub(crate) fn node_of_name(&self, normalized: &str) -> Option<u32> {
        self.names.get(normalized).copied()
    }

    /// Counting-sort `edges` into the CSR arrays and fill the side indexes.
    /// The offsets count adjacency slots; `degree` holds the *weighted* degree
    /// the push thresholds against.
    fn place(&mut self, edges: Vec<Resolved>) {
        let n = self.keys.len();
        let mut slots = vec![0_u32; n];
        let mut deg = vec![0_f64; n];
        for e in &edges {
            slots[ix(e.src)] += 1;
            slots[ix(e.dst)] += 1;
            deg[ix(e.src)] += e.weight;
            deg[ix(e.dst)] += e.weight;
        }
        self.offsets = Vec::with_capacity(n + 1);
        let mut off = 0_u32;
        self.offsets.push(0);
        for d in &slots {
            off += *d;
            self.offsets.push(off);
        }
        self.adj = vec![
            AdjEntry {
                other: 0,
                rel: 0,
                validity: 0,
                status: EdgeVisibility::Visible,
                note: NO_NOTE,
                src_side: false,
                synthetic: false,
                weight: 1.0,
            };
            ix(off)
        ];
        self.degree = deg;
        let mut cursor = self.offsets.clone();
        for p in edges {
            let (s, d) = (ix(p.src), ix(p.dst));
            let slot_s = ix(cursor[s]);
            let slot_d = ix(cursor[d]);
            cursor[s] += 1;
            cursor[d] += 1;
            self.adj[slot_s] = AdjEntry {
                other: p.dst,
                rel: p.rel,
                validity: p.validity,
                status: p.status,
                note: p.note,
                src_side: true,
                synthetic: p.synthetic,
                weight: p.weight,
            };
            self.adj[slot_d] = AdjEntry {
                other: p.src,
                rel: p.rel,
                validity: p.validity,
                status: p.status,
                note: p.note,
                src_side: false,
                synthetic: p.synthetic,
                weight: p.weight,
            };
            if p.note != NO_NOTE {
                self.edges_of_note
                    .entry(p.note)
                    .or_default()
                    .push(Loc::Base(slot_s as u32, slot_d as u32));
            }
            self.live_edges += 1;
        }
    }

    // --- interning -----------------------------------------------------

    fn intern_endpoint(&mut self, key: &str) -> u32 {
        if let Some(note_id) = key.strip_prefix("note:") {
            self.intern_note_id(note_id)
        } else {
            self.intern_entity(key, key)
        }
    }

    fn intern_note_id(&mut self, id: &str) -> u32 {
        let key = format!("note:{id}");
        if let Some(&n) = self.node_map.get(&key) {
            return n;
        }
        let n = self.keys.len() as u32;
        let ulid = Ulid::from_string(id).ok();
        self.keys.push(key.clone());
        self.display.push(key.clone());
        self.node_map.insert(key, n);
        self.node_note.push(ulid);
        // corrected by the status pass at build/patch time; Visible is the
        // fail-open default only for nodes the pass never sees
        self.note_vis.push(EdgeVisibility::Visible);
        self.degree.push(0.0);
        // After a build the offsets are live: a new node extends them with an
        // empty range so traversal stays in bounds.
        if !self.offsets.is_empty() {
            let last = self.offsets[self.offsets.len() - 1];
            self.offsets.push(last);
        }
        n
    }

    /// Entities are looked up case-insensitively (the SQL table is
    /// `COLLATE NOCASE`): the key is lowercase, the display keeps the case.
    fn intern_entity(&mut self, name: &str, display: &str) -> u32 {
        let key = name.to_lowercase();
        if let Some(&n) = self.node_map.get(&key) {
            return n;
        }
        let n = self.keys.len() as u32;
        self.keys.push(key.clone());
        self.display.push(display.to_string());
        self.node_map.insert(key, n);
        self.node_note.push(None);
        self.note_vis.push(EdgeVisibility::Visible);
        self.degree.push(0.0);
        if !self.offsets.is_empty() {
            let last = self.offsets[self.offsets.len() - 1];
            self.offsets.push(last);
        }
        n
    }

    fn intern_rel(&mut self, rel: &str) -> u32 {
        if let Some(&r) = self.rel_map.get(rel) {
            return r;
        }
        let r = self.rels.len() as u32;
        self.rels.push(rel.to_string());
        self.rel_map.insert(rel.to_string(), r);
        r
    }

    fn intern_window(&mut self, vf: &Option<String>, vt: &Option<String>) -> u16 {
        let parse = |s: &Option<String>, end: bool| -> i64 {
            s.as_deref()
                .and_then(|x| crate::format::Validity::parse(x).ok())
                .map(|v| {
                    let t = if end { v.end_exclusive() } else { v.start() };
                    t.unix_timestamp() * 1_000_000 + i64::from(t.nanosecond()) / 1_000
                })
                .unwrap_or(if end { i64::MAX } else { i64::MIN })
        };
        let w = (parse(vf, false), parse(vt, true));
        if let Some(&id) = self.window_map.get(&w) {
            return id;
        }
        let id = self.windows.len() as u16;
        self.windows.push(w);
        self.window_strings.push((vf.clone(), vt.clone()));
        self.window_map.insert(w, id);
        id
    }

    // --- lookups ---------------------------------------------------------

    /// The node of an entity by (any case) name.
    pub(crate) fn node_of_entity(&self, name: &str) -> Option<u32> {
        self.node_map.get(&name.to_lowercase()).copied()
    }

    /// The node of a note.
    pub(crate) fn node_of_note(&self, id: &Ulid) -> Option<u32> {
        self.node_map
            .get(&format!("note:{id}"))
            .copied()
            .filter(|&n| self.node_note[ix(n)].is_some())
    }

    /// The note a node stands for, when it is a note node.
    pub(crate) fn note_of_node(&self, node: u32) -> Option<Ulid> {
        self.node_note.get(ix(node)).copied().flatten()
    }

    /// Display name of a node (entity case, or the `note:` key).
    pub(crate) fn display_of(&self, node: u32) -> &str {
        self.display
            .get(ix(node))
            .map(String::as_str)
            .unwrap_or("?")
    }

    /// Relation id by name.
    pub(crate) fn rel_id(&self, rel: &str) -> Option<u32> {
        self.rel_map.get(rel).copied()
    }

    /// Relation name by id.
    pub(crate) fn rel_of(&self, rel: u32) -> &str {
        self.rels.get(ix(rel)).map(String::as_str).unwrap_or("?")
    }

    /// Live weighted degree of a node (the push threshold is `ε·deg`).
    pub(crate) fn degree_of(&self, node: u32) -> f64 {
        self.degree.get(ix(node)).copied().unwrap_or(0.0)
    }

    /// (nodes, real edges, estimated heap bytes) for the benchmark report.
    pub(crate) fn stats(&self) -> (usize, usize, usize) {
        let bytes = self.keys.len() * (std::mem::size_of::<String>() + 32)
            + self.node_note.len() * std::mem::size_of::<Option<Ulid>>()
            + self.adj.len() * std::mem::size_of::<AdjEntry>()
            + self.overlay.len() * std::mem::size_of::<OverlayEdge>();
        (self.keys.len(), self.real_edges, bytes)
    }

    /// The stored `valid_from`/`valid_to` strings of a window (display).
    pub(crate) fn valid_strings(&self, validity: u16) -> (Option<&str>, Option<&str>) {
        self.window_strings
            .get(usize::from(validity))
            .map(|(a, b)| (a.as_deref(), b.as_deref()))
            .unwrap_or((None, None))
    }

    /// Is the window `validity` in effect at `t` (µs)?
    fn valid_at(&self, validity: u16, t_us: Option<i64>) -> bool {
        let Some(t) = t_us else { return true };
        let (from, to) = self.windows[usize::from(validity)];
        from <= t && t < to
    }

    // --- adjacency -------------------------------------------------------

    /// Walk the live neighbours of `node`, filtered by visibility, validity
    /// at `as_of` and (optionally) relation. `with_declared` includes the
    /// bipartite projection (the push wants it, `neighbors`/`path` do not).
    pub(crate) fn steps<'a>(
        &'a self,
        node: u32,
        as_of: Option<OffsetDateTime>,
        include_quarantined: bool,
        rels: &[&str],
        with_declared: bool,
    ) -> impl Iterator<Item = Step<'a>> + 'a {
        let t_us =
            as_of.map(|t| t.unix_timestamp() * 1_000_000 + i64::from(t.nanosecond()) / 1_000);
        let rel_ids: Vec<u32> = rels
            .iter()
            .filter_map(|r| self.rel_map.get(*r).copied())
            .collect();
        let walked = node;
        let base = (self.offsets[ix(node)]..self.offsets[ix(node) + 1])
            .filter(move |&slot| !self.removed.contains(&slot))
            .map(move |slot| (slot, self.adj[ix(slot)]));
        let overlay = self
            .overlay_by_node
            .get(&walked)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter_map(move |&i| {
                let e = &self.overlay[ix(i)];
                e.alive.then_some((i, e))
            })
            .map(move |(i, e)| {
                let forward = e.from == walked;
                let other = if forward { e.to } else { e.from };
                (
                    i,
                    AdjEntry {
                        other,
                        rel: e.rel,
                        validity: e.validity,
                        status: e.status,
                        note: e.note,
                        src_side: forward,
                        synthetic: e.synthetic,
                        weight: e.weight,
                    },
                )
            });
        base.chain(overlay)
            .filter(move |(_, a)| with_declared || !a.synthetic)
            .filter(move |(_, a)| a.status.walkable(include_quarantined))
            // v0.6.8 (H1): the bin's one predicate in the walk — an
            // edge a note the bin holds declares is nobody's step, and
            // every walk (neighbors, path, the search's push) passes
            // here
            .filter(move |(_, a)| a.note == NO_NOTE || self.note_visible(a.note, include_quarantined))
            .filter(move |(_, a)| rel_ids.is_empty() || rel_ids.contains(&a.rel))
            .filter(move |(_, a)| self.valid_at(a.validity, t_us))
            .map(move |(_, a)| Step {
                node: a.other,
                rel: self.rels[ix(a.rel)].as_str(),
                note: (a.note != NO_NOTE).then_some(a.note),
                forward: a.src_side,
                validity: a.validity,
                weight: a.weight,
            })
    }

    // --- incremental maintenance ------------------------------------------

    /// True when the overlay is large enough that a full rebuild is cheaper.
    pub(crate) fn wants_rebuild(&self) -> bool {
        let pending = self.removed.len() + self.overlay.iter().filter(|e| e.alive).count();
        pending > (self.live_edges / 8).max(256)
    }

    /// Replay this cell's own write of note `id` over the index: every edge
    /// the note declared (and every manual edge citing it — they follow the
    /// note's lifecycle in the same write) is dropped and re-read from the
    /// table into the overlay, projection included.
    ///
    /// # Errors
    /// [`SearchError::Db`] on SQL failure.
    pub fn patch_note(&mut self, conn: &Connection, id: &Ulid) -> Result<(), SearchError> {
        let key = format!("note:{id}");
        if let Some(n) = self.node_map.get(&key).copied() {
            self.drop_note_edges(n);
        }
        // v0.4 (P1.4): the note's aliases follow its rewrite
        self.patch_names(conn, id)?;
        // Re-read what the table says now.
        let mut stmt = conn.prepare(
            "SELECT src, rel, dst, note_id, valid_from, valid_to, status FROM edges WHERE note_id = ?1",
        )?;
        let rows = stmt.query_map([id.to_string()], |r| {
            Ok(RawEdge {
                src: r.get(0)?,
                rel: r.get(1)?,
                dst: r.get(2)?,
                note_id: r.get(3)?,
                valid_from: r.get(4)?,
                valid_to: r.get(5)?,
                status: r.get(6)?,
            })
        })?;
        let mut fresh: Vec<RawEdge> = Vec::new();
        for row in rows {
            fresh.push(row?);
        }
        drop(stmt);
        // Resolve (interning needs &mut self), then the projection.
        let mut resolved: Vec<Resolved> = Vec::with_capacity(fresh.len() * 2);
        for e in &fresh {
            let src = self.intern_endpoint(&e.src);
            let dst = self.intern_endpoint(&e.dst);
            let note = e
                .note_id
                .as_deref()
                .map(|nid| self.intern_note_id(nid))
                .unwrap_or(NO_NOTE);
            let rel = self.intern_rel(&e.rel);
            let validity = self.intern_window(&e.valid_from, &e.valid_to);
            let status = EdgeVisibility::from_status(&e.status);
            let weight = walk_weight(&e.rel);
            resolved.push(Resolved {
                src,
                dst,
                rel,
                validity,
                status,
                note,
                synthetic: false,
                weight,
            });
        }
        let reals = resolved.clone();
        for e in &reals {
            if e.note == NO_NOTE || e.note == e.src || e.note == e.dst {
                continue;
            }
            for endpoint in [e.src, e.dst] {
                resolved.push(Resolved {
                    src: e.note,
                    dst: endpoint,
                    rel: e.rel,
                    validity: e.validity,
                    status: e.status,
                    note: NO_NOTE,
                    synthetic: true,
                    weight: e.weight,
                });
            }
        }
        self.real_edges += fresh.len();
        for p in resolved {
            let idx = self.overlay.len() as u32;
            self.overlay.push(OverlayEdge {
                from: p.src,
                to: p.dst,
                rel: p.rel,
                validity: p.validity,
                status: p.status,
                note: p.note,
                synthetic: p.synthetic,
                weight: p.weight,
                alive: true,
            });
            self.overlay_by_node.entry(p.src).or_default().push(idx);
            self.overlay_by_node.entry(p.dst).or_default().push(idx);
            if p.note != NO_NOTE {
                self.edges_of_note
                    .entry(p.note)
                    .or_default()
                    .push(Loc::Overlay(idx));
            }
            self.bump_degree(p.src, true);
            self.bump_degree(p.dst, true);
            self.live_edges += 1;
        }
        // v0.5.3 (P1): a rewrite may have changed the note's STATUS (a
        // quarantine, a confirm): the output filter must see it.
        // v0.6.8 (H1): the bin's one predicate — the ORIGINAL's delete
        // must hide the copy's node too, and a reopen must bring it
        // back: this update runs after every derivation of the note's
        // conflict set, so both flips land here
        if let Ok(note_status) = conn.query_row(
            "SELECT status FROM notes WHERE id = ?1",
            [id.to_string()],
            |r| r.get::<_, String>(0),
        ) {
            let in_bin = crate::cell::bin::note_in_bin(conn, &id.to_string())
                .map_err(|e| SearchError::Db(e.to_string()))?;
            let status = if in_bin { "deleted" } else { note_status.as_str() };
            if let Some(&n) = self.node_map.get(&format!("note:{id}"))
                && let Some(v) = self.note_vis.get_mut(ix(n))
            {
                *v = EdgeVisibility::from_status(status);
            }
            // the copies of THIS original flip with it (their own rows
            // did not change — the predicate did)
            let mut copy_stmt = conn
                .prepare("SELECT copy_id FROM conflict_copies WHERE original_id = ?1 AND open = 1")
                .map_err(|e| SearchError::Db(e.to_string()))?;
            let copies: Vec<String> = copy_stmt
                .query_map([id.to_string()], |r| r.get::<_, String>(0))
                .map_err(|e| SearchError::Db(e.to_string()))?
                .flatten()
                .collect();
            for copy in copies {
                // not in the bin: the copy's OWN state speaks (an
                // agent's copy is quarantined, a human's is active)
                let own: Option<String> = conn
                    .query_row(
                        "SELECT status FROM notes WHERE id = ?1",
                        [&copy],
                        |r| r.get(0),
                    )
                    .ok();
                let status: &str = if in_bin {
                    "deleted"
                } else {
                    own.as_deref().unwrap_or(note_status.as_str())
                };
                if let Some(&n) = self.node_map.get(&format!("note:{copy}"))
                    && let Some(v) = self.note_vis.get_mut(ix(n))
                {
                    *v = EdgeVisibility::from_status(status);
                }
            }
        }
        Ok(())
    }

    /// Remove every live edge the note node declares (base slots and overlay
    /// entries), degrees included.
    fn drop_note_edges(&mut self, note_node: u32) {
        if let Some(locs) = self.edges_of_note.remove(&note_node) {
            for loc in locs {
                match loc {
                    Loc::Base(s1, s2) => {
                        self.remove_slot(note_node, s1);
                        self.remove_slot(note_node, s2);
                        self.real_edges -= 1;
                    }
                    Loc::Overlay(i) => {
                        let (alive, from, to, synthetic) = {
                            let e = &mut self.overlay[ix(i)];
                            (e.alive, e.from, e.to, e.synthetic)
                        };
                        if alive {
                            self.overlay[ix(i)].alive = false;
                            self.bump_degree(from, false);
                            self.bump_degree(to, false);
                            self.live_edges -= 1;
                            if !synthetic {
                                self.real_edges -= 1;
                            }
                        }
                    }
                }
            }
        }
        // Overlay edges added by an earlier patch of the same note.
        let stale: Vec<u32> = self
            .overlay
            .iter()
            .enumerate()
            .filter(|(_, e)| e.alive && e.note == note_node)
            .map(|(i, _)| i as u32)
            .collect();
        for i in stale {
            let (from, to, synthetic) = {
                let e = &mut self.overlay[ix(i)];
                e.alive = false;
                (e.from, e.to, e.synthetic)
            };
            self.bump_degree(from, false);
            self.bump_degree(to, false);
            self.live_edges -= 1;
            if !synthetic {
                self.real_edges -= 1;
            }
        }
    }

    /// Remove one direction of a base edge from the live adjacency.
    fn remove_slot(&mut self, note_node: u32, slot: u32) {
        if self.removed.insert(slot) {
            let entry = self.adj[ix(slot)];
            self.bump_degree(entry.other, false);
            self.bump_degree(note_node, false);
            self.live_edges -= 1;
        }
    }

    fn bump_degree(&mut self, node: u32, up: bool) {
        if let Some(d) = self.degree.get_mut(ix(node)) {
            // removals may race with weights changing on re-patched notes;
            // the floor keeps the degree sane until the next rebuild.
            if up {
                *d += 1.0;
            } else {
                *d = (*d - 1.0).max(0.0);
            }
        }
    }

    // --- the walk -----------------------------------------------------------

    /// Andersen–Chung–Lang forward push from `seeds` (node, mass), masses
    /// summing to ~1. A node expands only while its residual clears
    /// `epsilon·degree`; `alpha` is the teleport share; outflow from nodes
    /// above `hub_theta` degrees is damped by `theta/degree` (kept from the
    /// prototype, where the proportional threshold already held hubs back).
    pub(crate) fn ppr(
        &self,
        seeds: &[(u32, f64)],
        alpha: f64,
        epsilon: f64,
        hub_theta: f64,
        as_of: Option<OffsetDateTime>,
        include_quarantined: bool,
    ) -> PushResult {
        let mut res: HashMap<u32, f64> = HashMap::new();
        let mut p: HashMap<u32, f64> = HashMap::new();
        // node -> (best predecessor, relation used) by largest single share
        let mut pred: HashMap<u32, (u32, u32, f64)> = HashMap::new();
        let mut queue: Vec<u32> = Vec::new();
        let seed_set: std::collections::HashSet<u32> = seeds.iter().map(|(n, _)| *n).collect();
        for (node, mass) in seeds {
            *res.entry(*node).or_default() += *mass;
            queue.push(*node);
        }
        let mut pushed_edges = 0_usize;
        while let Some(u) = queue.pop() {
            let deg = self.degree_of(u).max(1.0);
            let ru = *res.get(&u).unwrap_or(&0.0);
            if ru < epsilon * deg {
                continue;
            }
            res.insert(u, 0.0);
            *p.entry(u).or_default() += alpha * ru;
            let damp = if deg > hub_theta {
                hub_theta / deg
            } else {
                1.0
            };
            let unit = (1.0 - alpha) * ru * damp / deg;
            pushed_edges += deg as usize;
            for step in self.steps(u, as_of, include_quarantined, &[], true) {
                let v = step.node;
                let share = unit * step.weight;
                let before = *res.get(&v).unwrap_or(&0.0);
                let after = before + share;
                res.insert(v, after);
                if !seed_set.contains(&v) && pred.get(&v).is_none_or(|&(_, _, w)| share > w) {
                    pred.insert(
                        v,
                        (u, self.rel_map.get(step.rel).copied().unwrap_or(0), share),
                    );
                }
                let dv = self.degree_of(v).max(1.0);
                if before < epsilon * dv && after >= epsilon * dv {
                    queue.push(v);
                }
            }
        }
        // Notes only, seeds out, VISIBLE ones only (v0.5.3, P1): a hidden
        // note must not take a slot of the list nor move the max the scores
        // normalise by; sorted by settled mass.
        let mut notes: Vec<(u32, f64)> = p
            .iter()
            .filter(|(n, _)| {
                !seed_set.contains(n)
                    && self.node_note[ix(**n)].is_some()
                    && self.note_visible(**n, include_quarantined)
            })
            .map(|(n, m)| (*n, *m))
            .collect();
        notes.sort_by(|a, b| {
            b.1.partial_cmp(&a.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        PushResult {
            notes,
            pred,
            pushed_edges,
        }
    }
}

/// What a push produced.
pub(crate) struct PushResult {
    /// Note nodes by descending settled mass (seeds excluded).
    pub notes: Vec<(u32, f64)>,
    /// node -> (predecessor, relation id, share) for path reconstruction.
    pub pred: HashMap<u32, (u32, u32, f64)>,
    /// Edges pushed (diagnostics and the hub-damping measurement).
    pub pushed_edges: usize,
}
