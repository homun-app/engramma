// SPDX-License-Identifier: Apache-2.0

//! R6.1 (v0.6.1, revisore): `censor_shape_ok` checks the KEYS but not the
//! CONTENT of the strings. A live device could hide text in `old_id`,
//! `valid_to`, `old_updated_at`, `decided_at` or `target` — the shape is
//! the only boundary the identity no longer covers, so the strings must
//! be what they claim: the ids ULIDs, the dates instants of the format's
//! own rules (a `Validity` for the closings, an RFC 3339 `At` for the
//! timestamps).

use engramma_core::cell::ops::{self, OpKind, PayloadState};
use engramma_core::hlc::Hlc;
use ulid::Ulid;

fn op_of_kind(kind: OpKind, body: serde_json::Value) -> engramma_core::cell::ops::Op {
    let hlc = Hlc {
        physical_ms: 1_790_000_000_000,
        counter: 0,
        node: "censor000000".into(),
    };
    let mut op = ops::make_op(ops::OpParts {
        op_id: Ulid::from_string("01CENS0RSHAPE000000000000A").unwrap(/* t */),
        hlc,
        actor: "cli".into(),
        kind,
        note_id: Some(Ulid::from_string("01CENS0RSHAPE000000000000B").unwrap(/* t */)),
        base: None,
        body,
        salt: vec![3_u8; 16],
        device: None,
        sig: None,
    })
    .unwrap(/* t */);
    op.payload_state = PayloadState::Censored;
    op.salt = Some(ops::censor_salt(&op.op_id.to_string()).to_vec());
    op
}

/// The shapes the censor's own rule produces — all of them pass.
#[test]
fn the_censors_own_shapes_pass() {
    let a = op_of_kind(
        OpKind::Supersede,
        serde_json::json!({
            "old_id": "01CENS0RSHAPE000000000000C",
            "valid_to": "2026-11-14",
            "old_updated_at": "2026-11-14T10:00:00Z",
            "note": null,
        }),
    );
    assert!(ops::censor_shape_ok(&a), "a date-only valid_to is a Validity");
    let b = op_of_kind(
        OpKind::Supersede,
        serde_json::json!({
            "valid_to": "2026-11-14T00:00:00Z",
            "note": null,
        }),
    );
    assert!(ops::censor_shape_ok(&b), "a datetime valid_to is a Validity");
    let c = op_of_kind(
        OpKind::Decide,
        serde_json::json!({
            "proposal": "01CENS0RSHAPE000000000000C",
            "decision": "accept",
            "censored": true,
            "target": "01CENS0RSHAPE000000000000D",
            "target_closing": "2026-11-15",
            "decided_at": "2026-11-14T10:00:00Z",
        }),
    );
    assert!(ops::censor_shape_ok(&c));
    let d = op_of_kind(
        OpKind::RevertDecision,
        serde_json::json!({
            "censored": true,
            "decision": "revert",
            "decided_at": "2026-11-14T10:00:00Z",
        }),
    );
    assert!(ops::censor_shape_ok(&d));
}

/// Hidden text in the strings is REFUSED: the ids must be ULIDs and the
/// dates must parse as what they claim — whatever the keys say.
#[test]
fn hidden_text_in_the_strings_is_refused() {
    // a supersede whose old_id hides text
    let a = op_of_kind(
        OpKind::Supersede,
        serde_json::json!({
            "old_id": "il segreto quokkaviola nascosto nell'id",
            "note": null,
        }),
    );
    assert!(
        !ops::censor_shape_ok(&a),
        "a non-ULID old_id must be refused"
    );
    // a valid_to that is neither date nor datetime
    let b = op_of_kind(
        OpKind::Supersede,
        serde_json::json!({
            "valid_to": "quando pare a te",
            "note": null,
        }),
    );
    assert!(
        !ops::censor_shape_ok(&b),
        "a non-Validity valid_to must be refused"
    );
    // an old_updated_at that is not an RFC 3339 instant
    let c = op_of_kind(
        OpKind::Supersede,
        serde_json::json!({
            "old_updated_at": "ieri, più o meno",
            "note": null,
        }),
    );
    assert!(
        !ops::censor_shape_ok(&c),
        "a non-RFC-3339 old_updated_at must be refused"
    );
    // a decide whose target hides text
    let d = op_of_kind(
        OpKind::Decide,
        serde_json::json!({
            "proposal": "01CENS0RSHAPE000000000000C",
            "decision": "accept",
            "censored": true,
            "target": "testo nascosto nel bersaglio quokkaviola",
        }),
    );
    assert!(
        !ops::censor_shape_ok(&d),
        "a non-ULID target must be refused"
    );
    // a proposal that is not a ULID
    let e = op_of_kind(
        OpKind::Decide,
        serde_json::json!({
            "proposal": "non sono un ULID",
            "decision": "accept",
            "censored": true,
            "target": "01CENS0RSHAPE000000000000D",
        }),
    );
    assert!(
        !ops::censor_shape_ok(&e),
        "a non-ULID proposal must be refused"
    );
    // a decided_at that is not an instant
    let f = op_of_kind(
        OpKind::Decide,
        serde_json::json!({
            "proposal": "01CENS0RSHAPE000000000000C",
            "decision": "accept",
            "censored": true,
            "target": "01CENS0RSHAPE000000000000D",
            "decided_at": "adesso",
        }),
    );
    assert!(
        !ops::censor_shape_ok(&f),
        "a non-RFC-3339 decided_at must be refused"
    );
    // and a revert's decided_at too
    let g = op_of_kind(
        OpKind::RevertDecision,
        serde_json::json!({
            "censored": true,
            "decided_at": "chissà",
        }),
    );
    assert!(
        !ops::censor_shape_ok(&g),
        "a non-RFC-3339 decided_at in a revert must be refused"
    );
    // a target_closing that is not a Validity
    let h = op_of_kind(
        OpKind::Decide,
        serde_json::json!({
            "proposal": "01CENS0RSHAPE000000000000C",
            "decision": "accept",
            "censored": true,
            "target": "01CENS0RSHAPE000000000000D",
            "target_closing": "prima o poi",
        }),
    );
    assert!(
        !ops::censor_shape_ok(&h),
        "a non-Validity target_closing must be refused"
    );
}
