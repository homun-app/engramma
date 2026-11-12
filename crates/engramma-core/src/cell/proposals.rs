// SPDX-License-Identifier: Apache-2.0

//! v0.5 (P1): proposals, decisions and rollback — the engine side.
//!
//! A **proposal** is a note born in quarantine: the search never sees it,
//! the graph never seeds from it, until a **decision** says otherwise. The
//! decision applies in the same operation that records it, and a **revert**
//! brings the proposal back to the queue without rewriting history — never
//! over a later human correction (`CONFLICT`), never resurrecting purged
//! content (`purged`). The queue is a derived table maintained by the ops
//! themselves, so every replay rebuilds it and `verify` covers it.
//!
//! The threat model lives in DESIGN (v0.5): who proposes is an ACTOR in the
//! log, the declared `clientInfo` is registered but is never an identity,
//! and nothing here trusts the proposal's text — the duplicate test is
//! exact (normalized), the semantic hints are hints, never refusals.

use rusqlite::OptionalExtension;
use serde_json::Value;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::embed::Embedder;
use crate::error::CellError;
use crate::format::{Note, Status, Validity};
use crate::text;

use super::Cell;
use super::ops::OpKind;

// ---------------------------------------------------------------------------
// inputs and outputs
// ---------------------------------------------------------------------------

/// How a proposal relates to what the memory already holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProposalMode {
    /// A new fact.
    New,
    /// A correction of an existing note (the rettifica).
    Supersede(Ulid),
}

impl ProposalMode {
    /// `(wire name, target)` as stored in the op body and the table.
    fn parts(&self) -> (&'static str, Option<Ulid>) {
        match self {
            ProposalMode::New => ("new", None),
            ProposalMode::Supersede(t) => ("supersede", Some(*t)),
        }
    }
}

/// What the propose call may use beyond the cell itself: the embedding
/// model (semantic hints) and the configured reviewers (the automatic
/// decision). Both optional — a propose without a context queues.
#[derive(Clone, Copy, Default)]
pub struct ProposeContext<'a> {
    /// The embedder, when the process has the model loaded.
    pub embedder: Option<&'a dyn Embedder>,
    /// The parsed `reviewers.yaml`, when the data root has one.
    pub reviewers: Option<&'a crate::review::Reviewers>,
}

/// The fence: may the MACHINE take this decision? Whatever is not plainly
/// allowed lands on the human — a limit never refuses the proposal, it
/// only declines to decide it.
fn machine_may_decide(
    policy: &super::policy::Policy,
    note: &Note,
    target: Option<&Note>,
    accept: bool,
) -> bool {
    // never in automatic, either direction: external content, and the
    // corrections of notes the policy does not allow to close
    if note.confidence == crate::format::Confidence::External {
        return false;
    }
    if accept {
        if !policy.kinds.is_empty() && !policy.kinds.contains(&note.note_type) {
            return false;
        }
        if let Some(target) = target {
            use super::policy::AllowSupersede;
            let ok = match policy.allow_supersede {
                AllowSupersede::None => false,
                AllowSupersede::Inferred => target.confidence != crate::format::Confidence::Stated,
                AllowSupersede::Any => true,
            };
            if !ok {
                return false;
            }
            // a tombstone (or anything not living) is never closed by the
            // machine — a purged target does not reach here (it must exist)
            if target.status != Status::Active {
                return false;
            }
        }
    }
    true
}

/// What [`Cell::propose`] is asked to store.
#[derive(Debug, Clone)]
pub struct ProposalInput {
    /// The note. Its status is forced to `quarantined` and its source
    /// becomes the proposing actor — a proposal is never born active and
    /// never claims another provenance.
    pub note: Note,
    /// New fact, or the correction of a note.
    pub mode: ProposalMode,
    /// The client the assistant DECLARED (MCP `clientInfo`): registered in
    /// the queue, never treated as an identity.
    pub client: Option<String>,
    /// The assistant's stated motivation («l'utente l'ha detto»).
    pub reason: Option<String>,
}

/// What [`Cell::propose`] did.
#[derive(Debug, Clone)]
pub struct ProposalOutcome {
    /// `pending` (in the queue), `duplicate` (refused), or — when the
    /// cell's policy lets a reviewer decide inside the propose itself
    /// (P2) — `accepted`/`rejected`.
    pub status: ProposalStatus,
    /// The proposal's note id, when the proposal was written.
    pub id: Option<Ulid>,
    /// The existing twin, on `duplicate`.
    pub existing: Option<Ulid>,
    /// Up to 5 semantically similar ACTIVE notes, when the process has the
    /// model loaded. Hints for the human, never a refusal: the similarity
    /// threshold is not a measured rule.
    pub possible_duplicates: Vec<(Ulid, f32)>,
    /// Who decided, when the policy decided inside the propose (P2).
    pub decided_by: Option<String>,
    /// The decision's reason, when any.
    pub decision_reason: Option<String>,
}

/// The coarse status of a propose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalStatus {
    /// In the queue, invisible to the search.
    Pending,
    /// Refused: normalized text already present.
    Duplicate,
    /// Accepted by the policy's automatic reviewer (P2).
    Accepted,
    /// Rejected by the policy's automatic reviewer (P2).
    Rejected,
}

/// The state of a proposal in the queue's table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalState {
    /// In the queue, waiting for a decision.
    Pending,
    /// Accepted — the note is active (or the target is closed).
    Accepted,
    /// Rejected — the note is a tombstone, the text stays for the revert.
    Rejected,
    /// Its decision was cancelled: back in the queue, decidable again.
    Reverted,
}

impl ProposalState {
    /// Wire name.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProposalState::Pending => "pending",
            ProposalState::Accepted => "accepted",
            ProposalState::Rejected => "rejected",
            ProposalState::Reverted => "reverted",
        }
    }

    fn parse(s: &str) -> Result<Self, CellError> {
        Ok(match s {
            "pending" => ProposalState::Pending,
            "accepted" => ProposalState::Accepted,
            "rejected" => ProposalState::Rejected,
            "reverted" => ProposalState::Reverted,
            other => {
                return Err(CellError::Db(format!("bad proposal state `{other}`")));
            }
        })
    }

    /// Whether the proposal currently sits in the queue (pending again, or
    /// waiting after a revert): decidable, invisible.
    #[must_use]
    pub fn in_queue(&self) -> bool {
        matches!(self, ProposalState::Pending | ProposalState::Reverted)
    }
}

/// One row of the proposals queue.
#[derive(Debug, Clone)]
pub struct ProposalRow {
    /// The proposed note.
    pub id: Ulid,
    /// New fact, or correction of `target`.
    pub mode: ProposalMode,
    /// The superseded note, when a correction.
    pub target: Option<Ulid>,
    /// Who proposed (the propose op's actor, e.g. `mcp:claude-desktop`).
    pub proposer: String,
    /// The declared client, registered, never an identity.
    pub client: Option<String>,
    /// The assistant's stated motivation.
    pub reason: Option<String>,
    /// Queue state.
    pub state: ProposalState,
    /// When the proposal was made.
    pub proposed_at: String,
    /// Who decided, when decided.
    pub decided_by: Option<String>,
    /// When, when decided.
    pub decided_at: Option<String>,
    /// Why, when decided.
    pub decision_reason: Option<String>,
    /// The reviewer metadata of an automatic decision (name, model, prompt
    /// hash, declared confidence), when any.
    pub review: Option<Value>,
    /// An automatic review was ATTEMPTED and FAILED (error, timeout,
    /// unreadable answer): the proposal waits for a human or a retry. An
    /// escalation is a choice — this stays false.
    pub auto_failed: bool,
    /// The current note.
    pub note: Note,
}

/// Filter for [`Cell::proposals`].
#[derive(Debug, Clone, Default)]
pub struct ProposalsFilter {
    /// Only this state.
    pub state: Option<ProposalState>,
    /// Only this proposer (exact or prefix — `mcp:` catches every MCP actor).
    pub by: Option<String>,
    /// Proposed since (inclusive).
    pub since: Option<OffsetDateTime>,
    /// Proposed until (inclusive).
    pub until: Option<OffsetDateTime>,
}

/// The metadata of an automatic reviewer's decision (P2): recorded with the
/// decision, so the history can say WHICH model, under WHICH prompt,
/// declared WHAT confidence.
#[derive(Debug, Clone)]
pub struct ReviewMeta {
    /// The reviewer's configured name.
    pub reviewer: String,
    /// The model it used, when it says so.
    pub model: Option<String>,
    /// blake3 of the prompt version the decision was taken under.
    pub prompt_hash: String,
    /// The declared confidence, 0..=1.
    pub confidence: f64,
}

impl ReviewMeta {
    fn to_json(&self) -> Value {
        serde_json::json!({
            "name": self.reviewer,
            "model": self.model,
            "prompt_hash": self.prompt_hash,
            "confidence": self.confidence,
        })
    }
}

/// What a decision does.
#[derive(Debug, Clone)]
// one `Note` per accept-with-edit, once per call: the size is the API's
// convenience, not a hot path
#[allow(clippy::large_enum_variant)]
pub enum DecisionAction {
    /// Activate the note. `edited` carries the text the human fixed while
    /// accepting (accept-with-edit); `None` accepts as proposed.
    Accept { edited: Option<Note> },
    /// Tombstone the note — the text stays, so the decision can be undone.
    Reject,
}

/// What [`Cell::decide`] is asked to do.
#[derive(Debug, Clone)]
pub struct DecisionInput {
    /// Accept (with or without edit) or reject.
    pub action: DecisionAction,
    /// The motivation, kept with the decision.
    pub reason: Option<String>,
    /// The automatic reviewer's metadata, when the decider IS a reviewer
    /// (actor `reviewer:<name>`); `None` for human decisions.
    pub review: Option<ReviewMeta>,
}

/// One row of the decisions registry.
#[derive(Debug, Clone)]
pub struct DecisionRow {
    /// The decide op.
    pub op_id: Ulid,
    /// The proposal it decided.
    pub proposal: Ulid,
    /// The proposal's supersede target, when a correction.
    pub target: Option<Ulid>,
    /// `accept` or `reject`.
    pub decision: String,
    /// Who decided (`user@cli`, `user@review`, `reviewer:<name>`…).
    pub actor: String,
    /// When.
    pub decided_at: String,
    /// Why.
    pub reason: Option<String>,
    /// The reviewer metadata, when an automatic decision.
    pub review: Option<Value>,
    /// Whether the decision was cancelled (and by which revert op).
    pub reverted_by: Option<Ulid>,
}

impl DecisionRow {
    /// The decision's current standing.
    #[must_use]
    pub fn state(&self) -> DecisionState {
        if self.reverted_by.is_some() {
            DecisionState::Reverted
        } else {
            DecisionState::Active
        }
    }
}

/// Whether a decision still stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionState {
    /// The decision stands.
    Active,
    /// Cancelled by a revert.
    Reverted,
}

/// Filter for [`Cell::decisions`].
#[derive(Debug, Clone, Default)]
pub struct DecisionsFilter {
    /// Only this actor, exact or PREFIX (`reviewer:` — the bulk cancel of
    /// one reviewer's decisions is a filter, nothing more).
    pub by: Option<String>,
    /// Decided since (inclusive).
    pub since: Option<OffsetDateTime>,
    /// Decided until (inclusive).
    pub until: Option<OffsetDateTime>,
    /// Only this standing.
    pub state: Option<DecisionState>,
}

/// What a single revert did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevertOutcome {
    /// The decision was cancelled: the proposal is back in the queue and a
    /// closed target, when any, was reopened.
    Reverted {
        /// The cancelled decision op.
        decision: Ulid,
        /// The proposal back in the queue.
        proposal: Ulid,
    },
    /// The content was purged after the decision: nothing can come back.
    /// The revert is still recorded in the log — the audit stands — but no
    /// state changes.
    Purged {
        /// The cancelled decision op.
        decision: Ulid,
    },
}

/// Filter for [`Cell::revert_decisions`].
#[derive(Debug, Clone, Default)]
pub struct RevertFilter {
    /// Only decisions of this actor, exact or prefix (`reviewer:x`).
    pub by: Option<String>,
    /// Decided since (inclusive).
    pub since: Option<OffsetDateTime>,
    /// Decided until (inclusive).
    pub until: Option<OffsetDateTime>,
}

/// What [`Cell::revert_decisions`] did (and what a dry run would do).
#[derive(Debug, Clone, Default)]
pub struct RevertReport {
    /// Decisions cancelled.
    pub reverted: usize,
    /// Refused: a later operation touched the note (or the target) after
    /// the decision. Each entry names the operations in the way.
    pub conflicts: Vec<String>,
    /// Decisions whose content was purged: cancelled in the log, nothing
    /// returned.
    pub purged: usize,
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// The normalized text a duplicate test runs on: title and body together,
/// through [`text::normalize_for_duplicate`].
fn dup_key(title: Option<&str>, body: &str) -> String {
    let joined = match title {
        Some(t) if !t.trim().is_empty() => format!("{t}\n{body}"),
        _ => body.to_string(),
    };
    text::normalize_for_duplicate(&joined)
}

/// One raw `proposals` row, before parsing (the query is one flat tuple).
type ProposalRaw = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
);

/// One raw `decisions` row (with the joined proposal target), before parsing.
type DecisionRaw = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Lexical bound for the fixed-width `Z` timestamps the queue stores.
fn bound(v: &Option<OffsetDateTime>) -> Option<String> {
    v.map(|t| crate::format::At(t).to_db_string())
}

fn row_in_range(at: &Option<String>, since: &Option<String>, until: &Option<String>) -> bool {
    let Some(at) = at else {
        return true;
    };
    if let Some(s) = since
        && at.as_str() < s.as_str()
    {
        return false;
    }
    if let Some(u) = until
        && at.as_str() > u.as_str()
    {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// the Cell API
// ---------------------------------------------------------------------------

impl Cell {
    /// Propose a fact. The note is born quarantined — in the queue, invisible
    /// to the search — unless the cell's policy lets a configured reviewer
    /// decide inside this very call (P2). A normalized duplicate of an
    /// active note or of a queued proposal is refused without writing
    /// anything; when `embedder` is given AND the cell's ready model matches
    /// it, the outcome lists up to 5 semantic near-neighbours as HINTS.
    ///
    /// # Errors
    /// [`CellError::NoteNotFound`] for an unknown supersede target;
    /// [`CellError::Invalid`] when the note already exists;
    /// [`CellError::Db`] on SQL failures.
    pub fn propose(
        &self,
        input: &ProposalInput,
        actor: &str,
        ctx: &ProposeContext<'_>,
    ) -> Result<ProposalOutcome, CellError> {
        let mut note = input.note.clone();
        note.status = Status::Quarantined;
        note.source = actor.to_string();

        // The policy is the first word: a cell nobody opened to proposals
        // (write: off, the default) refuses them on every surface.
        let policy = self.policy_of()?;
        if policy.write == super::policy::WritePolicy::Off {
            return Err(CellError::Invalid(
                "the cell does not accept proposals (policy `write: off`): \
                 `engramma policy set` opens it"
                    .into(),
            ));
        }

        // The semantic neighbours embed OUTSIDE every lock (the engine's
        // rule), before anything is written: a failed or absent embedding
        // never fails the propose — they are hints for the human AND
        // context for the reviewer.
        let neighbours: Vec<crate::search::VecCand> = match ctx.embedder {
            Some(e) => {
                let text = crate::embed::note_embedding_text(note.title.as_deref(), &note.body);
                e.embed(&[&text], crate::embed::TextKind::Query)
                    .ok()
                    .and_then(|vs| vs.into_iter().next())
                    .and_then(|v| self.semantic_neighbours(&v, e.model_id()).ok())
                    .unwrap_or_default()
            }
            None => Vec::new(),
        };
        let possible: Vec<(Ulid, f32)> = neighbours
            .iter()
            .filter_map(|c| Some((Ulid::from_string(&c.id).ok()?, c.cosine as f32)))
            .collect();

        // The creation-time checks, under the cell's write lock.
        let target_note = {
            let _guard = self
                .write_lock
                .lock()
                .map_err(|_| CellError::Db("write lock poisoned".into()))?;
            let inner = self.lock()?;
            if super::ops::note_exists(&inner.conn, &note.id)? {
                return Err(CellError::Invalid(format!(
                    "note {} already exists",
                    note.id
                )));
            }
            // The duplicate test: normalized text among the ACTIVE notes and
            // the QUEUE (pending or reverted proposals). Rejected proposals
            // are tombstones, not the queue: proposing the same fact again
            // after a reject is legitimate.
            let key = dup_key(note.title.as_deref(), &note.body);
            let mut stmt = inner.conn.prepare(
                "SELECT id, title, body FROM notes WHERE status = 'active'
                 UNION ALL
                 SELECT n.id, n.title, n.body FROM notes n
                 JOIN proposals p ON p.note_id = n.id
                 WHERE p.state IN ('pending', 'reverted')",
            )?;
            let rows: Vec<(String, Option<String>, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .flatten()
                .collect();
            drop(stmt);
            for (id, title, body) in rows {
                if dup_key(title.as_deref(), &body) == key {
                    return Ok(ProposalOutcome {
                        status: ProposalStatus::Duplicate,
                        id: None,
                        existing: Ulid::from_string(&id).ok(),
                        possible_duplicates: possible,
                        decided_by: None,
                        decision_reason: None,
                    });
                }
            }
            // The supersede target must exist (and its text rides with the
            // reviewer's request).
            match input.mode {
                ProposalMode::Supersede(target) => {
                    if !super::ops::note_exists(&inner.conn, &target)? {
                        return Err(CellError::NoteNotFound(target.to_string()));
                    }
                    let tx = inner.conn.unchecked_transaction()?;
                    Some(super::get_note_tx(&tx, &target)?)
                }
                ProposalMode::New => None,
            }
        };

        // The automatic review (write: auto) runs INSIDE the propose, with
        // the reviewer's own time limit — and outside every lock: a slow
        // model must not freeze the cell. Every failure lands on the human.
        let mut auto_failed = false;
        let mut review_error: Option<String> = None;
        let mut machine: Option<(bool, String, ReviewMeta)> = None; // (accept?, reason, meta)
        if policy.write == super::policy::WritePolicy::Auto {
            let reviewer_name = policy.reviewer.clone().unwrap_or_default(); // Policy::parse guarantees a name for auto
            let reviewer_actor = format!("reviewer:{reviewer_name}");
            let def = ctx.reviewers.and_then(|rs| rs.get(&reviewer_name));
            let breaker = self.recent_auto_decisions(&reviewer_actor)?;
            match def {
                None => {
                    auto_failed = true;
                    review_error = Some(format!("reviewer `{reviewer_name}` is not configured"));
                }
                Some(_) if breaker >= u64::from(policy.max_auto_per_hour) => {
                    // THE BREAKER: the machine stops, the human takes over.
                    // Not a failure — a deliberate stop.
                }
                Some(def) => {
                    let request = crate::review::ReviewRequest {
                        cell: self
                            .meta()
                            .ok()
                            .and_then(|m| m.get("cell_name").cloned())
                            .unwrap_or_default(),
                        proposal: serde_json::json!({
                            "text": note.body,
                            "title": note.title,
                            "kind": note.note_type.as_str(),
                            "tags": note.tags,
                            "confidence": note.confidence.as_str(),
                            "mode": match input.mode {
                                ProposalMode::New => "new",
                                ProposalMode::Supersede(_) => "supersede",
                            },
                            "reason": input.reason,
                        }),
                        target_text: target_note.as_ref().map(|t| {
                            crate::embed::note_embedding_text(t.title.as_deref(), &t.body)
                        }),
                        similar: neighbours
                            .iter()
                            .map(|c| {
                                (
                                    c.id.clone(),
                                    crate::embed::note_embedding_text(
                                        c.note.title.as_deref(),
                                        &c.note.body,
                                    ),
                                )
                            })
                            .collect(),
                        policy: policy.summary(),
                    };
                    match def.review(&request) {
                        crate::review::ReviewVerdict::Failed { error } => {
                            auto_failed = true;
                            review_error = Some(error);
                        }
                        crate::review::ReviewVerdict::Escalate { .. } => {
                            // the reviewer's own choice: the human, no marker
                        }
                        verdict => {
                            let (accept, reason, confidence) = match verdict {
                                crate::review::ReviewVerdict::Accept { reason, confidence } => {
                                    (true, reason, confidence)
                                }
                                crate::review::ReviewVerdict::Reject { reason, confidence } => {
                                    (false, reason, confidence)
                                }
                                other => unreachable!("the other arms returned above: {other:?}"),
                            };
                            let meta = ReviewMeta {
                                reviewer: reviewer_name.clone(),
                                model: def.model_name(),
                                prompt_hash: crate::review::prompt_hash(),
                                confidence,
                            };
                            // the limits: whatever is not plainly allowed
                            // goes to the human — never a refusal, never an
                            // acceptance. The declared confidence gates BOTH
                            // directions: a doubtful decision is no decision.
                            if confidence >= policy.min_confidence
                                && machine_may_decide(&policy, &note, target_note.as_ref(), accept)
                            {
                                machine = Some((accept, reason, meta));
                            }
                        }
                    }
                }
            }
        }

        // The propose op itself. `auto_failed` and the error ride in the
        // body: the queue row is derived from THIS op, so the marker replays.
        let proposed_at = self.now_at()?;
        let (mode, target) = input.mode.parts();
        let payload = serde_json::json!({
            "note": note.to_json(),
            "mode": mode,
            "target": target.map(|t| t.to_string()),
            "client": input.client,
            "reason": input.reason,
            "proposed_at": proposed_at.to_db_string(),
            "auto_failed": auto_failed,
            "review_error": review_error,
        });
        {
            let _guard = self
                .write_lock
                .lock()
                .map_err(|_| CellError::Db("write lock poisoned".into()))?;
            // the reviewer's window (write: auto) is deliberately
            // unguarded — a slow model must not freeze the cell — so the
            // duplicate check runs AGAIN here, under the lock, right
            // before the write: whatever slipped past the first check
            // while a review was running is caught now (found by the
            // hand-proof session of the v0.5 REPORT)
            if let Some(existing) = self.duplicate_of(&note)? {
                return Ok(ProposalOutcome {
                    status: ProposalStatus::Duplicate,
                    id: None,
                    existing: Some(existing),
                    possible_duplicates: possible,
                    decided_by: None,
                    decision_reason: None,
                });
            }
            let mut inner = self.lock()?;
            super::execute_op(&mut inner, OpKind::Propose, Some(note.id), actor, payload)?;
        }
        // A machine decision applies right after its proposal, as its own
        // operation (actor `reviewer:<name>`, with the review metadata).
        if let Some((accept, reason, meta)) = machine {
            let reviewer_actor = format!("reviewer:{}", meta.reviewer);
            let input = DecisionInput {
                action: if accept {
                    DecisionAction::Accept { edited: None }
                } else {
                    DecisionAction::Reject
                },
                reason: Some(reason.clone()),
                review: Some(meta),
            };
            self.decide(&note.id, &input, &reviewer_actor)?;
            return Ok(ProposalOutcome {
                status: if accept {
                    ProposalStatus::Accepted
                } else {
                    ProposalStatus::Rejected
                },
                // the queue's key is the NOTE, not the op: every caller
                // (CLI, review page, MCP) shows and accepts this one
                id: Some(note.id),
                existing: None,
                possible_duplicates: possible,
                decided_by: Some(reviewer_actor),
                decision_reason: Some(reason),
            });
        }
        Ok(ProposalOutcome {
            status: ProposalStatus::Pending,
            id: Some(note.id),
            existing: None,
            possible_duplicates: possible,
            decided_by: None,
            decision_reason: None,
        })
    }

    /// The normalized-duplicate check: `Some(existing)` when the text is
    /// already among the ACTIVE notes or the QUEUE.
    fn duplicate_of(&self, note: &Note) -> Result<Option<Ulid>, CellError> {
        let inner = self.lock()?;
        let key = dup_key(note.title.as_deref(), &note.body);
        let mut stmt = inner.conn.prepare(
            "SELECT id, title, body FROM notes WHERE status = 'active'
             UNION ALL
             SELECT n.id, n.title, n.body FROM notes n
             JOIN proposals p ON p.note_id = n.id
             WHERE p.state IN ('pending', 'reverted')",
        )?;
        let rows: Vec<(String, Option<String>, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .flatten()
            .collect();
        drop(stmt);
        for (id, title, body) in rows {
            if dup_key(title.as_deref(), &body) == key {
                return Ok(Ulid::from_string(&id).ok());
            }
        }
        Ok(None)
    }

    /// The decisions an actor (a reviewer) took in the last hour — the
    /// breaker's count.
    fn recent_auto_decisions(&self, actor: &str) -> Result<u64, CellError> {
        let since = OffsetDateTime::now_utc() - time::Duration::hours(1);
        let inner = self.lock()?;
        let n: i64 = inner.conn.query_row(
            "SELECT COUNT(*) FROM decisions WHERE actor = ?1 AND decided_at >= ?2",
            rusqlite::params![actor, crate::format::At(since).to_db_string()],
            |r| r.get(0),
        )?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// Up to 5 semantically similar ACTIVE notes for a propose (the hints
    /// for the human, the context for the reviewer). Builds the vector
    /// index when the cell has a ready model; an absent or mismatched
    /// model returns nothing.
    fn semantic_neighbours(
        &self,
        qv: &[f32],
        model: &str,
    ) -> Result<Vec<crate::search::VecCand>, CellError> {
        let mut inner = self.lock()?;
        let Some((ready, _dim)) =
            crate::search::ensure_index(&mut inner).map_err(|e| CellError::Db(e.to_string()))?
        else {
            return Ok(Vec::new());
        };
        if ready != model {
            return Ok(Vec::new());
        }
        let filters = crate::search::Filters {
            include_quarantined: false,
            ..Default::default()
        };
        let cands = crate::search::vector_candidates(
            &inner.conn,
            &inner,
            qv,
            5,
            &filters,
            OffsetDateTime::now_utc(),
            &ready,
        )
        .map_err(|e| CellError::Db(e.to_string()))?;
        Ok(cands)
    }

    /// The proposals queue (or its history, by state).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failures.
    pub fn proposals(&self, filter: &ProposalsFilter) -> Result<Vec<ProposalRow>, CellError> {
        let inner = self.lock()?;
        let since = bound(&filter.since);
        let until = bound(&filter.until);
        let mut stmt = inner.conn.prepare(
            "SELECT p.note_id, p.mode, p.target, p.proposer, p.client, p.reason, p.state,
                    p.proposed_at, p.decided_by, p.decided_at, p.decision_reason, p.review_json,
                    p.auto_failed
             FROM proposals p ORDER BY p.proposed_at, p.note_id",
        )?;
        let raw: Vec<ProposalRaw> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                    r.get(10)?,
                    r.get(11)?,
                    r.get::<_, i64>(12)? != 0,
                ))
            })?
            .flatten()
            .collect();
        drop(stmt);
        let mut out = Vec::new();
        for (
            note_id,
            mode,
            target,
            proposer,
            client,
            reason,
            state,
            proposed_at,
            decided_by,
            decided_at,
            decision_reason,
            review_json,
            auto_failed,
        ) in raw
        {
            let state = ProposalState::parse(&state)?;
            if let Some(want) = filter.state
                && want != state
            {
                continue;
            }
            if let Some(by) = &filter.by
                && !proposer.starts_with(by.as_str())
            {
                continue;
            }
            if !row_in_range(&Some(proposed_at.clone()), &since, &until) {
                continue;
            }
            let note = {
                let tx = inner.conn.unchecked_transaction()?;
                super::get_note_tx(
                    &tx,
                    &Ulid::from_string(&note_id).map_err(|_| CellError::Db("bad id".into()))?,
                )?
            };
            let target_id = target.as_deref().and_then(|t| Ulid::from_string(t).ok());
            out.push(ProposalRow {
                id: Ulid::from_string(&note_id).map_err(|_| CellError::Db("bad id".into()))?,
                mode: if mode == "supersede" {
                    ProposalMode::Supersede(
                        target_id.ok_or_else(|| CellError::Db("bad target".into()))?,
                    )
                } else {
                    ProposalMode::New
                },
                target: target_id,
                proposer,
                client,
                reason,
                state,
                proposed_at,
                decided_by,
                decided_at,
                decision_reason,
                review: review_json
                    .map(|s| serde_json::from_str::<Value>(&s))
                    .transpose()
                    .map_err(|e| CellError::Db(format!("bad review_json: {e}")))?,
                auto_failed,
                note,
            });
        }
        Ok(out)
    }

    /// Decide a proposal. The effect applies in the same operation that
    /// records the decision: `accept` activates the note (and closes the
    /// supersede target), `reject` tombstones it — the text stays, so the
    /// decision can be reverted. Returns the decide op's id.
    ///
    /// # Errors
    /// [`CellError::Invalid`] when the note is not a decidable proposal;
    /// [`CellError::NoteNotFound`] for a missing supersede target;
    /// [`CellError::Db`] on SQL failures.
    pub fn decide(&self, id: &Ulid, input: &DecisionInput, actor: &str) -> Result<Ulid, CellError> {
        let decided_at = self.now_at()?;
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let (target, target_before, target_closing, edited) = {
            let inner = self.lock()?;
            let meta = super::ops::proposal_meta(&inner.conn, id)?;
            let Some((mode, target, state)) = meta else {
                return Err(CellError::Invalid(format!("note {id} is not a proposal")));
            };
            if !ProposalState::parse(&state)?.in_queue() {
                return Err(CellError::Invalid(format!(
                    "the proposal is `{state}`; revert its decision before deciding again"
                )));
            }
            let edited = match &input.action {
                DecisionAction::Accept { edited } => {
                    if let Some(e) = edited {
                        if e.id != *id {
                            return Err(CellError::Invalid(format!(
                                "the edited note must keep the proposal's id ({})",
                                e.id
                            )));
                        }
                        if e.status == Status::Deleted {
                            return Err(CellError::Invalid(
                                "the edited proposal cannot be a tombstone".into(),
                            ));
                        }
                    }
                    edited.clone()
                }
                DecisionAction::Reject => None,
            };
            // the supersede target: it must exist now, and the decision
            // records what it was (the revert restores exactly that)
            let mut target_before = None;
            let mut target_closing = None;
            if mode == "supersede" {
                let Some(target) = target else {
                    return Err(CellError::Db("supersede proposal without target".into()));
                };
                let row = super::ops::read_note_status(&inner.conn, &target)?
                    .ok_or_else(|| CellError::NoteNotFound(target.to_string()))?;
                target_before = Some(serde_json::json!({
                    "id": target.to_string(),
                    "status": row.0.as_str(),
                    "valid_to": row.1.as_ref().map(|v| v.to_db_string()),
                }));
                // the closing this decision will apply: the (possibly
                // edited) note's valid_from, else the decision date —
                // recorded so the purge's censor can keep it verbatim
                let vf = edited
                    .as_ref()
                    .and_then(|e| e.valid_from)
                    .or_else(|| self.note_valid_of(&inner, id).ok().flatten())
                    .unwrap_or_else(|| Validity(decided_at.get().date(), None));
                target_closing = Some(vf.to_db_string());
            }
            (target, target_before, target_closing, edited)
        };
        let decision = match input.action {
            DecisionAction::Accept { .. } => "accept",
            DecisionAction::Reject => "reject",
        };
        let payload = serde_json::json!({
            "proposal": id.to_string(),
            "decision": decision,
            "reason": input.reason,
            "edited": edited.map(|n| n.to_json()),
            "target": target.map(|t| t.to_string()),
            "target_before": target_before,
            "target_closing": target_closing,
            "review": input.review.as_ref().map(ReviewMeta::to_json),
            "decided_at": decided_at.to_db_string(),
        });
        let mut inner = self.lock()?;
        super::execute_op(&mut inner, OpKind::Decide, Some(*id), actor, payload)
    }

    /// The `valid_from` of a stored note (creation-time read for the closing
    /// a decision will apply).
    fn note_valid_of(
        &self,
        inner: &super::Inner,
        id: &Ulid,
    ) -> Result<Option<Validity>, CellError> {
        let s: Option<String> = inner
            .conn
            .query_row(
                "SELECT valid_from FROM notes WHERE id = ?1",
                [id.to_string()],
                |r| r.get(0),
            )
            .optional()?;
        Ok(s.and_then(|v| Validity::parse(&v).ok()))
    }

    /// The decisions registry, newest last.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failures.
    pub fn decisions(&self, filter: &DecisionsFilter) -> Result<Vec<DecisionRow>, CellError> {
        let inner = self.lock()?;
        let since = bound(&filter.since);
        let until = bound(&filter.until);
        let mut stmt = inner.conn.prepare(
            "SELECT d.op_id, d.proposal, d.decision, d.actor, d.decided_at, d.reason,
                    d.review_json, d.reverted_by, p.target
             FROM decisions d LEFT JOIN proposals p ON p.note_id = d.proposal
             ORDER BY d.decided_at, d.op_id",
        )?;
        let raw: Vec<DecisionRaw> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                ))
            })?
            .flatten()
            .collect();
        drop(stmt);
        let mut out = Vec::new();
        for (
            op_id,
            proposal,
            decision,
            actor,
            decided_at,
            reason,
            review_json,
            reverted_by,
            target,
        ) in raw
        {
            if let Some(by) = &filter.by
                && !actor.starts_with(by.as_str())
            {
                continue;
            }
            if !row_in_range(&Some(decided_at.clone()), &since, &until) {
                continue;
            }
            let reverted_by = reverted_by.and_then(|s| Ulid::from_string(&s).ok());
            if let Some(want) = filter.state {
                let is = if reverted_by.is_some() {
                    DecisionState::Reverted
                } else {
                    DecisionState::Active
                };
                if want != is {
                    continue;
                }
            }
            out.push(DecisionRow {
                op_id: Ulid::from_string(&op_id)
                    .map_err(|_| CellError::Db("bad decision op id".into()))?,
                proposal: Ulid::from_string(&proposal)
                    .map_err(|_| CellError::Db("bad proposal id".into()))?,
                target: target.and_then(|t| Ulid::from_string(&t).ok()),
                decision,
                actor,
                decided_at,
                reason,
                review: review_json
                    .map(|s| serde_json::from_str::<Value>(&s))
                    .transpose()
                    .map_err(|e| CellError::Db(format!("bad review_json: {e}")))?,
                reverted_by,
            });
        }
        Ok(out)
    }

    /// Cancel one decision: the proposal returns to the queue (quarantined
    /// again, invisible), a closed supersede target reopens exactly as it
    /// was, and the log gains a `revert_decision` op — history is never
    /// rewritten. Refused with [`CellError::Conflict`] when any operation
    /// touched the note (or the target) after the decision; when the content
    /// was purged, the outcome is [`RevertOutcome::Purged`] and nothing
    /// comes back (the audit op is still written).
    ///
    /// # Errors
    /// [`CellError::Invalid`] for an unknown or already-reverted decision;
    /// [`CellError::Conflict`] over later operations; [`CellError::Db`] on
    /// SQL failures.
    pub fn revert_decision(
        &self,
        decision_op: &Ulid,
        reason: &str,
        actor: &str,
    ) -> Result<RevertOutcome, CellError> {
        // BEFORE the locks: now_at takes the inner mutex itself (the same
        // pattern as decide — a non-reentrant mutex under a held guard is a
        // self-deadlock)
        let decided_at = self.now_at()?;
        let _guard = self
            .write_lock
            .lock()
            .map_err(|_| CellError::Db("write lock poisoned".into()))?;
        let mut inner = self.lock()?;
        match classify_revert(&inner.conn, decision_op)? {
            Classified::Purged { note } => {
                let payload = serde_json::json!({
                    "decision": decision_op.to_string(),
                    "reason": reason,
                    "decided_at": decided_at.to_db_string(),
                    "purged": true,
                });
                super::execute_op(
                    &mut inner,
                    OpKind::RevertDecision,
                    Some(note),
                    actor,
                    payload,
                )?;
                Ok(RevertOutcome::Purged {
                    decision: *decision_op,
                })
            }
            Classified::AlreadyReverted => Err(CellError::Invalid(format!(
                "decision {decision_op} was already reverted"
            ))),
            Classified::Conflict(what) => Err(CellError::Conflict(what)),
            Classified::Revertable {
                note,
                restore_target,
            } => {
                let payload = serde_json::json!({
                    "decision": decision_op.to_string(),
                    "reason": reason,
                    "decided_at": decided_at.to_db_string(),
                    "restore_target": restore_target,
                    "purged": false,
                });
                super::execute_op(
                    &mut inner,
                    OpKind::RevertDecision,
                    Some(note),
                    actor,
                    payload,
                )?;
                Ok(RevertOutcome::Reverted {
                    decision: *decision_op,
                    proposal: note,
                })
            }
        }
    }

    /// Cancel many decisions at once — the bulk undo of one reviewer (actor
    /// or prefix, e.g. `reviewer:x`), optionally bounded by decision dates.
    /// With `dry_run` nothing is written: the report says what WOULD happen.
    /// Every decision is judged on its own — a conflict or a purge stops
    /// THAT revert, not the run.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failures.
    pub fn revert_decisions(
        &self,
        filter: &RevertFilter,
        dry_run: bool,
    ) -> Result<RevertReport, CellError> {
        let since = bound(&filter.since);
        let until = bound(&filter.until);
        // The scan runs over the OPS, not the registry: a purge that took a
        // decision's rows away does not take its op row, and those decisions
        // are exactly the `purged` the report must count. The date of a
        // redacted decision falls back to its HLC (the envelope survives).
        let candidates: Vec<(String, String, Option<String>)> = {
            let inner = self.lock()?;
            let mut stmt = inner.conn.prepare(
                "SELECT o.op_id, o.actor, p.body FROM ops o
                 LEFT JOIN op_payloads p ON p.op_id = o.op_id
                 WHERE o.kind = 'decide' ORDER BY o.seq",
            )?;
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .flatten()
                .collect()
        };
        let mut report = RevertReport::default();
        for (op_id, actor, body) in candidates {
            if let Some(by) = &filter.by
                && !actor.starts_with(by.as_str())
            {
                continue;
            }
            let decided_at: Option<String> = body
                .as_deref()
                .and_then(|b| serde_json::from_str::<Value>(b).ok())
                .and_then(|v| {
                    v.get("decided_at")
                        .and_then(Value::as_str)
                        .map(String::from)
                });
            if !row_in_range(&decided_at, &since, &until) {
                continue;
            }
            let Ok(op) = Ulid::from_string(&op_id) else {
                continue;
            };
            let classified = {
                let inner = self.lock()?;
                classify_revert(&inner.conn, &op)?
            };
            match classified {
                Classified::Purged { .. } => report.purged += 1,
                Classified::AlreadyReverted => {}
                Classified::Conflict(what) => {
                    report.conflicts.push(format!("decision {op_id}: {what}"))
                }
                Classified::Revertable { .. } => {
                    report.reverted += 1;
                    if !dry_run {
                        // re-classified inside, under the write lock: between
                        // the two looks nothing changed (this thread owns the
                        // loop), and the single-decision path IS the check
                        self.revert_decision(&op, "annullamento in blocco", "user@bulk-revert")?;
                    }
                }
            }
        }
        Ok(report)
    }

    /// Retry the FAILED automatic reviews (`engramma proposals review`):
    /// only proposals that are pending AND marked `auto_failed` — never the
    /// ones the reviewer deliberately passed to the human. A retry that
    /// decides, decides with the current policy and the current reviewer
    /// (`reviewer:<name>`); one that fails again simply stays marked.
    /// Returns how many were decided.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failures; [`CellError::Invalid`] when the
    /// policy refuses (it must be `write: auto` to retry).
    pub fn review_failed(&self, ctx: &ProposeContext<'_>) -> Result<usize, CellError> {
        let policy = self.policy_of()?;
        if policy.write != super::policy::WritePolicy::Auto {
            return Err(CellError::Invalid(
                "the retry wants the policy `write: auto`".into(),
            ));
        }
        let failed: Vec<Ulid> = self
            .proposals(&ProposalsFilter {
                state: Some(ProposalState::Pending),
                ..Default::default()
            })?
            .into_iter()
            .filter(|p| p.auto_failed)
            .map(|p| p.id)
            .collect();
        let mut decided = 0;
        for id in failed {
            // the same path a fresh propose walks, minus the new note: the
            // reviewer sees the stored proposal
            let row = self
                .proposals(&ProposalsFilter::default())?
                .into_iter()
                .find(|p| p.id == id)
                .ok_or_else(|| CellError::Db("the queue row vanished".into()))?;
            let target_note = match row.mode {
                ProposalMode::Supersede(t) => Some(self.get(&t)?),
                ProposalMode::New => None,
            };
            let reviewer_name = policy.reviewer.clone().unwrap_or_default();
            let actor = format!("reviewer:{reviewer_name}");
            let Some(def) = ctx.reviewers.and_then(|rs| rs.get(&reviewer_name)) else {
                continue; // still not configured: stays failed, the CLI says so
            };
            if self.recent_auto_decisions(&actor)? >= u64::from(policy.max_auto_per_hour) {
                break; // the breaker holds on retries too
            }
            let neighbours: Vec<crate::search::VecCand> = match ctx.embedder {
                Some(e) => {
                    let text = crate::embed::note_embedding_text(
                        row.note.title.as_deref(),
                        &row.note.body,
                    );
                    e.embed(&[&text], crate::embed::TextKind::Query)
                        .ok()
                        .and_then(|vs| vs.into_iter().next())
                        .and_then(|v| self.semantic_neighbours(&v, e.model_id()).ok())
                        .unwrap_or_default()
                }
                None => Vec::new(),
            };
            let request = crate::review::ReviewRequest {
                cell: self
                    .meta()
                    .ok()
                    .and_then(|m| m.get("cell_name").cloned())
                    .unwrap_or_default(),
                proposal: serde_json::json!({
                    "text": row.note.body,
                    "title": row.note.title,
                    "kind": row.note.note_type.as_str(),
                    "tags": row.note.tags,
                    "confidence": row.note.confidence.as_str(),
                    "mode": match row.mode {
                        ProposalMode::New => "new",
                        ProposalMode::Supersede(_) => "supersede",
                    },
                    "reason": row.reason,
                }),
                target_text: target_note
                    .as_ref()
                    .map(|t| crate::embed::note_embedding_text(t.title.as_deref(), &t.body)),
                similar: neighbours
                    .iter()
                    .map(|c| {
                        (
                            c.id.clone(),
                            crate::embed::note_embedding_text(
                                c.note.title.as_deref(),
                                &c.note.body,
                            ),
                        )
                    })
                    .collect(),
                policy: policy.summary(),
            };
            let meta = ReviewMeta {
                reviewer: reviewer_name,
                model: def.model_name(),
                prompt_hash: crate::review::prompt_hash(),
                confidence: 0.0,
            };
            match def.review(&request) {
                crate::review::ReviewVerdict::Accept { reason, confidence } => {
                    if confidence >= policy.min_confidence
                        && machine_may_decide(&policy, &row.note, target_note.as_ref(), true)
                    {
                        let meta = ReviewMeta { confidence, ..meta };
                        self.decide(
                            &id,
                            &DecisionInput {
                                action: DecisionAction::Accept { edited: None },
                                reason: Some(reason),
                                review: Some(meta),
                            },
                            &actor,
                        )?;
                        decided += 1;
                    }
                }
                crate::review::ReviewVerdict::Reject { reason, confidence } => {
                    if confidence >= policy.min_confidence
                        && machine_may_decide(&policy, &row.note, target_note.as_ref(), false)
                    {
                        let meta = ReviewMeta { confidence, ..meta };
                        self.decide(
                            &id,
                            &DecisionInput {
                                action: DecisionAction::Reject,
                                reason: Some(reason),
                                review: Some(meta),
                            },
                            &actor,
                        )?;
                        decided += 1;
                    }
                }
                crate::review::ReviewVerdict::Escalate { .. }
                | crate::review::ReviewVerdict::Failed { .. } => {}
            }
        }
        Ok(decided)
    }

    /// Set the cell's policy. The op and the schema arrived with P1; the
    /// SEMANTICS (who may propose, who reviews, the breaker) are P2 — this
    /// is the writing surface P2 builds on, so the history says who allowed
    /// what, and when. `policy` is the whole document: it replaces the
    /// previous one.
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failures.
    pub fn set_policy(&self, policy: Value, actor: &str) -> Result<(), CellError> {
        self.execute(OpKind::SetPolicy, None, actor, policy)
    }

    /// The current policy document, when a `set_policy` op ever ran (P2
    /// defines the fields; `None` means the defaults).
    ///
    /// # Errors
    /// [`CellError::Db`] on SQL failures.
    pub fn policy(&self) -> Result<Option<Value>, CellError> {
        let inner = self.lock()?;
        let s: Option<String> = inner
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'policy'", [], |r| {
                r.get(0)
            })
            .optional()?;
        s.map(|v| serde_json::from_str(&v))
            .transpose()
            .map_err(|e| CellError::Db(format!("bad policy: {e}")))
    }
}

/// What a revert would find on the ground, read without writing anything.
#[derive(Debug)]
enum Classified {
    /// The revert can go: the proposal returns to the queue; `restore_target`
    /// is the recorded pre-decision state of the target, when any.
    Revertable {
        note: Ulid,
        restore_target: Option<Value>,
    },
    /// Cancelled already: nothing to do.
    AlreadyReverted,
    /// Later operations stand in the way (the message names them).
    Conflict(String),
    /// The content was purged: cancelled in the log, nothing comes back.
    Purged { note: Ulid },
}

fn classify_revert(
    conn: &rusqlite::Connection,
    decision_op: &Ulid,
) -> Result<Classified, CellError> {
    // the decision op itself: seq, note, and (when it survives) its body
    let row: Option<(i64, Option<String>, Option<String>)> = conn
        .query_row(
            "SELECT o.seq, o.note_id, p.body FROM ops o
             LEFT JOIN op_payloads p ON p.op_id = o.op_id
             WHERE o.op_id = ?1 AND o.kind = 'decide'",
            [decision_op.to_string()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((seq, note_id, body)) = row else {
        return Err(CellError::Invalid(format!(
            "{decision_op} is not a decision"
        )));
    };
    let note = note_id
        .and_then(|s| Ulid::from_string(&s).ok())
        .ok_or_else(|| CellError::Db("decision without note".into()))?;
    let body: Option<Value> = body
        .map(|b| serde_json::from_str(&b))
        .transpose()
        .map_err(|e| CellError::Db(format!("bad decide payload: {e}")))?;
    let target: Option<Ulid> = body
        .as_ref()
        .and_then(|b| b.get("target"))
        .and_then(Value::as_str)
        .and_then(|s| Ulid::from_string(s).ok());

    // the registry row: absent means a purge already took this decision
    let reverted_by: Option<Option<String>> = conn
        .query_row(
            "SELECT reverted_by FROM decisions WHERE op_id = ?1",
            [decision_op.to_string()],
            |r| r.get(0),
        )
        .optional()?;
    // everything that touched the note or the target after the decision
    let later: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT seq, kind, actor FROM ops
             WHERE seq > ?1 AND (note_id = ?2 OR note_id = ?3) ORDER BY seq",
        )?;
        stmt.query_map(
            rusqlite::params![
                seq,
                note.to_string(),
                target.map(|t| t.to_string()).unwrap_or_default()
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?
        .flatten()
        .collect()
    };

    let purged = reverted_by.is_none()
        || later.iter().any(|(_, kind, _)| kind == "purge")
        || !super::ops::note_exists(conn, &note)?;
    if purged {
        return Ok(Classified::Purged { note });
    }
    if matches!(reverted_by, Some(Some(_))) {
        return Ok(Classified::AlreadyReverted);
    }
    let blockers: Vec<String> = later
        .iter()
        .map(|(s, kind, by)| format!("seq {s} `{kind}` by {by}"))
        .collect();
    if !blockers.is_empty() {
        return Ok(Classified::Conflict(format!(
            "the proposal (or its target) has later operations: {}; \
             the revert would land over them",
            blockers.join(", ")
        )));
    }
    // what the decision's own payload recorded: the target's state before it
    // closed — the only honest way to reopen it. Without the payload (a
    // compaction forgot it) the revert is refused rather than guessed.
    let restore_target: Option<Value> = match target {
        Some(_) => {
            let before = body
                .as_ref()
                .and_then(|b| b.get("target_before"))
                .filter(|v| !v.is_null())
                .cloned();
            match before {
                Some(v) => Some(v),
                None => {
                    return Err(CellError::Invalid(format!(
                        "the payload of decision {decision_op} was compacted away: the target \
                         of {note} cannot be reopened exactly; supersede again instead"
                    )));
                }
            }
        }
        None => None,
    };
    Ok(Classified::Revertable {
        note,
        restore_target,
    })
}
