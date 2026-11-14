// SPDX-License-Identifier: Apache-2.0

//! The wire form of one operation (v0.6.1, P1): the JSON an envelope's
//! plaintext carries between devices. Everything the receiver needs to
//! rebuild the operation and verify it — its identity, its causality, its
//! author's signature, its payload in whichever state the sender's copy
//! holds it (`whole`, `redacted`, `censored` — v0.6.1, P0: the payload may
//! differ between copies, the identity never does).
//!
//! The form is versioned by the session's protocol version: a receiver
//! refuses a frame it cannot read in full, and every missing or malformed
//! field is a refusal, never a default.

use engramma_core::cell::ops::{self, MerkleStep, Op, OpKind, OpParts, PayloadState};
use engramma_core::hlc::Hlc;
use ulid::Ulid;

use crate::{Error, Result};

/// The wire form's own version (independent of the session protocol's).
pub const WIRE_VERSION: u64 = 2;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Result<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::Malformed("bad hex".into()));
    }
    (0..text.len() / 2)
        .map(|i| {
            u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                .map_err(|e| Error::Malformed(format!("bad hex: {e}")))
        })
        .collect()
}

/// Encode one operation for the wire.
#[must_use]
pub fn op_wire(op: &Op) -> String {
    op_value(op).to_string()
}

/// The operation as the wire's JSON object.
fn op_value(op: &Op) -> serde_json::Value {
    serde_json::json!({
        "v": WIRE_VERSION,
        "op_id": op.op_id.to_string(),
        "hlc": op.hlc.to_string(),
        "actor": op.actor,
        "kind": op.kind.as_str(),
        "note_id": op.note_id.map(|n| n.to_string()),
        "base": op.base,
        "payload_digest": op.payload_digest,
        "payload_state": op.payload_state.as_str(),
        "content_hash": op.content_hash,
        "device": op.device,
        "sig": op.sig,
        "body": op.body,
        "salt": op.salt.as_deref().map(hex),
    })
}

/// Decode one operation from the wire. Every field must be present and
/// well formed; a frame that misses anything, or claims a version this
/// build does not speak, is refused — never defaulted.
///
/// # Errors
/// [`Error::Malformed`] for anything that is not exactly an operation.
pub fn op_from_wire(plaintext: &[u8]) -> Result<Op> {
    let v: serde_json::Value = serde_json::from_slice(plaintext)
        .map_err(|e| Error::Malformed(format!("bad op wire: {e}")))?;
    if v.get("v").and_then(serde_json::Value::as_u64) != Some(WIRE_VERSION) {
        return Err(Error::Malformed(format!(
            "op wire version {:?} is not {WIRE_VERSION}",
            v.get("v")
        )));
    }
    let get_s = |k: &str| -> Result<String> {
        v.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| Error::Malformed(format!("the op wire misses `{k}`")))
    };
    let op_id = Ulid::from_string(&get_s("op_id")?)
        .map_err(|e| Error::Malformed(format!("bad op_id: {e}")))?;
    let hlc = Hlc::parse(&get_s("hlc")?).map_err(|e| Error::Malformed(format!("bad hlc: {e}")))?;
    let kind =
        OpKind::parse(&get_s("kind")?).map_err(|e| Error::Malformed(format!("bad kind: {e}")))?;
    let note_id = match v.get("note_id") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => {
            Some(Ulid::from_string(s).map_err(|e| Error::Malformed(format!("bad note_id: {e}")))?)
        }
        _ => return Err(Error::Malformed("note_id must be a string or null".into())),
    };
    let base = match v.get("base") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s)) => Some(s.clone()),
        _ => return Err(Error::Malformed("base must be a string or null".into())),
    };
    let payload_state = PayloadState::parse(&get_s("payload_state")?)
        .map_err(|e| Error::Malformed(e.to_string()))?;
    // the body and the salt follow the state: whole and censored carry
    // both, redacted carries neither
    let (body, salt) = match payload_state {
        PayloadState::Whole | PayloadState::Censored => {
            let body = v
                .get("body")
                .cloned()
                .filter(|b| b.is_object())
                .ok_or_else(|| Error::Malformed("the op wire misses its body".into()))?;
            let salt_hex = get_s("salt")?;
            let salt = unhex(&salt_hex)?;
            if salt.len() != 16 {
                return Err(Error::Malformed("the salt must be 16 bytes".into()));
            }
            (Some(body), Some(salt))
        }
        PayloadState::Redacted => {
            if v.get("body").is_some_and(|b| !b.is_null())
                || v.get("salt").is_some_and(|s| !s.is_null())
            {
                return Err(Error::Malformed(
                    "a redacted operation carries no body or salt".into(),
                ));
            }
            (None, None)
        }
    };
    // a whole or censored op is rebuilt through `make_op` (which digests
    // the body — the receiver compares below); a redacted one carries no
    // body at all and is rebuilt directly
    let payload_digest = get_s("payload_digest")?;
    let content_hash = get_s("content_hash")?;
    let mut op = if body.is_some() {
        ops::make_op(OpParts {
            op_id,
            hlc,
            actor: get_s("actor")?,
            kind,
            note_id,
            base,
            body: body.unwrap_or_default(),
            salt: salt.unwrap_or_default(),
            device: get_opt_string(&v, "device")?,
            sig: get_opt_string(&v, "sig")?,
        })
        .map_err(|e| Error::Malformed(format!("op rebuild: {e}")))?
    } else {
        Op {
            seq: 0,
            op_id,
            hlc,
            actor: get_s("actor")?,
            kind,
            note_id,
            base,
            body: None,
            salt: None,
            payload_state: PayloadState::Redacted,
            payload_digest: payload_digest.clone(),
            content_hash: content_hash.clone(),
            prev_hash: String::new(),
            hash: String::new(),
            device: get_opt_string(&v, "device")?,
            sig: get_opt_string(&v, "sig")?,
        }
    };
    // the identity travels whole: the receiver's ingest recomputes it from
    // these very fields and refuses a row that claims what it is not
    op.payload_digest = payload_digest;
    op.content_hash = content_hash;
    op.payload_state = payload_state;
    Ok(op)
}

/// The ENVELOPE form (what a sealed box actually carries): the operation
/// plus, when the operation is unsigned, the Merkle proof that it sits
/// inside an `adopt`'s root — the receiver verifies it against the root
/// the adopt recorded, holding none of the other leaves.
#[must_use]
pub fn envelope_wire(op: &Op, proof: Option<&[MerkleStep]>) -> String {
    let inner = op_value(op);
    serde_json::json!({
        "v": WIRE_VERSION,
        "op": inner,
        "proof": proof.map(|path| {
            path.iter()
                .map(|step| serde_json::json!({
                    "sibling": step.sibling.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                    "right": step.right,
                }))
                .collect::<Vec<_>>()
        }),
    })
    .to_string()
}

/// Parse the envelope form: the operation and its adopt proof, when it
/// carries one.
///
/// # Errors
/// [`Error::Malformed`] for anything that is not exactly an envelope.
pub fn envelope_from_wire(plaintext: &[u8]) -> Result<(Op, Option<Vec<MerkleStep>>)> {
    let v: serde_json::Value = serde_json::from_slice(plaintext)
        .map_err(|e| Error::Malformed(format!("bad envelope wire: {e}")))?;
    if v.get("v").and_then(serde_json::Value::as_u64) != Some(WIRE_VERSION) {
        return Err(Error::Malformed("envelope version mismatch".into()));
    }
    let inner = v
        .get("op")
        .ok_or_else(|| Error::Malformed("the envelope misses its op".into()))?;
    let op = op_from_wire(inner.to_string().as_bytes())?;
    let proof = match v.get("proof") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Array(steps)) => {
            let mut path = Vec::with_capacity(steps.len());
            for step in steps {
                let sibling_hex = step
                    .get("sibling")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| Error::Malformed("a proof step misses its sibling".into()))?;
                let mut sibling = [0_u8; 32];
                if sibling_hex.len() != 64 || !sibling_hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(Error::Malformed("a proof sibling must be 32 bytes".into()));
                }
                for (i, chunk) in sibling_hex.as_bytes().chunks(2).enumerate() {
                    let hi = (chunk[0] as char).to_digit(16).unwrap_or(0) as u8;
                    let lo = (chunk[1] as char).to_digit(16).unwrap_or(0) as u8;
                    sibling[i] = (hi << 4) | lo;
                }
                let right = step
                    .get("right")
                    .and_then(serde_json::Value::as_bool)
                    .ok_or_else(|| Error::Malformed("a proof step misses its side".into()))?;
                path.push(MerkleStep { sibling, right });
            }
            Some(path)
        }
        _ => {
            return Err(Error::Malformed(
                "the proof must be an array or null".into(),
            ));
        }
    };
    Ok((op, proof))
}

fn get_opt_string(v: &serde_json::Value, k: &str) -> Result<Option<String>> {
    match v.get(k) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(Error::Malformed(format!("`{k}` must be a string or null"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(state: PayloadState) -> Op {
        let mut op = ops::make_op(OpParts {
            op_id: Ulid::from_string("01F7ZQ6VTSATW2BQ5Z9R0P1S8T").unwrap(/* t */),
            hlc: Hlc::parse("01a17720ca0000000007.0123456789ab").unwrap(/* t */),
            actor: "device-a".into(),
            kind: OpKind::PatchNote,
            note_id: Some(Ulid::from_string("01F7ZQ6VTSATW2BQ5Z9R0P1S8T").unwrap(/* t */)),
            base: Some("9".repeat(64)),
            body: serde_json::json!({ "set": { "body": "corpo" } }),
            salt: vec![7_u8; 16],
            device: Some("a".repeat(64)),
            sig: Some("c2lnbmF0dXJh".into()),
        })
        .unwrap(/* t */);
        match state {
            PayloadState::Whole => {}
            PayloadState::Redacted => {
                op.body = None;
                op.salt = None;
            }
            PayloadState::Censored => {
                // any censored body: the shape rule is the censor's own
                op.kind = OpKind::Supersede;
                op.body = Some(
                    serde_json::json!({ "note": null, "old_id": "01F7ZQ6VTSATW2BQ5Z9R0P1S8T" }),
                );
                op.salt = Some(ops::censor_salt(&op.op_id.to_string()).to_vec());
            }
        }
        op.payload_state = state;
        op
    }

    #[test]
    fn the_wire_round_trips_every_payload_state() {
        for state in [
            PayloadState::Whole,
            PayloadState::Redacted,
            PayloadState::Censored,
        ] {
            let op = sample(state);
            let back = op_from_wire(op_wire(&op).as_bytes()).unwrap(/* t */);
            assert_eq!(back.op_id, op.op_id);
            assert_eq!(back.hlc, op.hlc);
            assert_eq!(back.actor, op.actor);
            assert_eq!(back.kind.as_str(), op.kind.as_str());
            assert_eq!(back.note_id, op.note_id);
            assert_eq!(back.base, op.base);
            assert_eq!(back.payload_digest, op.payload_digest);
            assert_eq!(back.payload_state, op.payload_state);
            assert_eq!(back.content_hash, op.content_hash);
            assert_eq!(back.device, op.device);
            assert_eq!(back.sig, op.sig);
            assert_eq!(back.body, op.body);
            assert_eq!(back.salt, op.salt);
        }
    }

    #[test]
    fn the_envelope_form_round_trips_with_and_without_a_proof() {
        use engramma_core::cell::ops::{merkle_prove, merkle_root, merkle_root_with};
        let op = sample(PayloadState::Whole);
        // no proof (a signed op carries none)
        let (back, proof) = envelope_from_wire(envelope_wire(&op, None).as_bytes())
            .unwrap(/* t */);
        assert_eq!(back.op_id, op.op_id);
        assert_eq!(back.content_hash, op.content_hash);
        assert!(proof.is_none());
        // with a proof over a small set: the proof walks to the root
        let mut hashes: Vec<String> = (0..5)
            .map(|i| blake3::hash(&[i; 8]).to_hex().to_string())
            .chain(std::iter::once(op.content_hash.clone()))
            .collect();
        let root = merkle_root(&mut hashes);
        let path = merkle_prove(&mut hashes, &op.content_hash).unwrap(/* t */);
        assert_eq!(
            merkle_root_with(&op.content_hash, &path).as_deref(),
            Some(root.as_str())
        );
        let (back, proof) = envelope_from_wire(envelope_wire(&op, Some(&path)).as_bytes())
            .unwrap(/* t */);
        assert_eq!(back.content_hash, op.content_hash);
        let proof = proof.expect(/* sent */ "the proof travels");
        assert_eq!(
            merkle_root_with(&back.content_hash, &proof).as_deref(),
            Some(root.as_str())
        );
        // a broken step is refused by the parse, a broken walk by the root
        let mut broken = serde_json::from_str::<serde_json::Value>(
            &envelope_wire(&op, Some(&path)),
        )
        .unwrap(/* t */);
        broken["proof"][0]["sibling"] = "zz".into();
        assert!(envelope_from_wire(broken.to_string().as_bytes()).is_err());
    }

    #[test]
    fn a_frame_from_another_version_is_refused() {
        let op = sample(PayloadState::Whole);
        let mut frame = op_wire(&op);
        frame = frame.replace("\"v\":2", "\"v\":1");
        assert!(op_from_wire(frame.as_bytes()).is_err());
        let frame = frame.replace("\"v\":1", "\"v\":3");
        assert!(op_from_wire(frame.as_bytes()).is_err());
    }

    #[test]
    fn a_missing_or_malformed_field_is_refused_not_defaulted() {
        let op = sample(PayloadState::Whole);
        let good: serde_json::Value = serde_json::from_str(&op_wire(&op)).unwrap(/* t */);
        for key in [
            "op_id",
            "hlc",
            "actor",
            "kind",
            "payload_digest",
            "payload_state",
            "content_hash",
            "body",
            "salt",
        ] {
            let mut broken = good.clone();
            broken
                .as_object_mut()
                .unwrap(/* t */)
                .remove(key);
            assert!(
                op_from_wire(broken.to_string().as_bytes()).is_err(),
                "a frame without `{key}` must be refused"
            );
        }
        // malformed values
        let mut broken = good.clone();
        broken["op_id"] = "not-a-ulid".into();
        assert!(op_from_wire(broken.to_string().as_bytes()).is_err());
        let mut broken = good.clone();
        broken["kind"] = "not-a-kind".into();
        assert!(op_from_wire(broken.to_string().as_bytes()).is_err());
        let mut broken = good.clone();
        broken["payload_state"] = "half".into();
        assert!(op_from_wire(broken.to_string().as_bytes()).is_err());
        let mut broken = good.clone();
        broken["salt"] = "zz".into();
        assert!(op_from_wire(broken.to_string().as_bytes()).is_err());
        // a redacted op that smuggles a body
        let mut smuggler = good.clone();
        smuggler["payload_state"] = "redacted".into();
        assert!(op_from_wire(smuggler.to_string().as_bytes()).is_err());
        // garbage outright
        assert!(op_from_wire(b"not json").is_err());
        assert!(op_from_wire(b"[]").is_err());
    }
}
