// SPDX-License-Identifier: Apache-2.0

//! v0.6 (P3): the two ways in — the TICKET (inviter's address + 128-bit
//! secret, ten minutes, once) and the SHORT CODE (channel + two words of
//! the fixed list, SPAKE2, one attempt) — and the one confirmation both
//! share: the six-digit SAS, printed on both screens, confirmed by the
//! human.

use engramma_sync::pairing::{ShortCode, SpakeHalf, Ticket, six_digits, transcript_mac};

#[test]
fn a_ticket_round_trips_and_expires() {
    let node = [7_u8; 32];
    let now = 1_000_000_u64;
    let t = Ticket::mint(
        node,
        "https://relay.example",
        &["192.168.1.10:1234".into()],
        now,
    )
    .unwrap(/* t */);
    let text = t.encode();
    assert!(text.starts_with("engramma-invite-v1."));
    let back = Ticket::decode(&text).unwrap(/* t */);
    assert_eq!(back.node_id, node);
    assert_eq!(back.relay, "https://relay.example");
    assert_eq!(back.addrs, vec!["192.168.1.10:1234".to_string()]);
    assert_eq!(back.secret, t.secret);
    assert!(back.is_valid(now + 60_000));
    assert!(!back.is_valid(now + 11 * 60_000), "ten minutes, no more");
    assert!(Ticket::decode("engramma-invite-v2.xyz").is_err());
}

#[test]
fn the_short_code_speaks_the_vocabulary() {
    let c = ShortCode::generate().unwrap(/* t */);
    let text = c.encode();
    let back = ShortCode::decode(&text).unwrap(/* t */);
    assert_eq!(back, c);
    // a word outside the list is not a code
    assert!(ShortCode::decode("7-asdfgh-qwerty").is_err());
    assert!(ShortCode::decode("7-tigre").is_err());
    // two random codes are (astronomically likely) different
    let other = ShortCode::generate().unwrap(/* t */);
    assert_ne!(c.password(), other.password());
}

#[test]
fn spake2_agrees_and_a_wrong_code_fails() {
    let code = ShortCode::from_parts(7, "tigre", "lago");
    let pw = code.password();
    let (a, msg_a) = SpakeHalf::begin_a(&pw);
    let (b, msg_b) = SpakeHalf::begin_b(&pw);
    let key_a = a.finish(&msg_b).unwrap(/* t */);
    let key_b = b.finish(&msg_a).unwrap(/* t */);
    assert_eq!(key_a, key_b, "both sides hold the same secret");

    // SPAKE2 is a PAKE: a wrong guess still produces a shared key — just a
    // DIFFERENT one on each side. The protocol's defense is that the two
    // sides then confirm (the SAS), and ONE attempt per code: the keys
    // disagreeing is the whole tell
    let wrong = ShortCode::from_parts(7, "tigre", "port");
    let (a2, msg_a2) = SpakeHalf::begin_a(&wrong.password());
    let (b2, msg_b2) = SpakeHalf::begin_b(&pw);
    let k2a = a2.finish(&msg_b2).unwrap(/* t */);
    let k2b = b2.finish(&msg_a2).unwrap(/* t */);
    assert_ne!(k2a, k2b, "a wrong code never agrees on a secret");
}

#[test]
fn both_screens_print_the_same_six_digits() {
    let code = ShortCode::from_parts(3, "gatto", "neve");
    let pw = code.password();
    let (a, msg_a) = SpakeHalf::begin_a(&pw);
    let (b, msg_b) = SpakeHalf::begin_b(&pw);
    let key_a = a.finish(&msg_b).unwrap(/* t */);
    let key_b = b.finish(&msg_a).unwrap(/* t */);
    // the transcript on each side is A‖B in the same order
    let sas_a = six_digits(&key_a, &[msg_a.clone(), msg_b.clone()].concat());
    let sas_b = six_digits(&key_b, &[msg_a, msg_b].concat());
    assert_eq!(sas_a, sas_b);
    assert_eq!(sas_a.len(), 6);
    assert!(sas_a.bytes().all(|b| b.is_ascii_digit()));
    // a different exchange prints different digits
    let other = ShortCode::from_parts(4, "gatto", "neve");
    let (c, mc) = SpakeHalf::begin_a(&other.password());
    let (_d, md) = SpakeHalf::begin_b(&other.password());
    let kc = c.finish(&md).unwrap(/* t */);
    let sas_c = six_digits(&kc, &[mc, md].concat());
    assert_ne!(sas_a, sas_c);
}

#[test]
fn the_inviter_proves_itself_over_the_transcript_not_in_the_clear() {
    // the ticket's secret never crosses; what crosses is its MAC over the
    // connection transcript — and only the true inviter can compute it
    let secret = [9_u8; 16];
    let transcript = b"whatever the connection said, byte for byte";
    let mac = transcript_mac(&secret, transcript);
    assert_eq!(mac, transcript_mac(&secret, transcript));
    assert_ne!(mac, transcript_mac(&[8_u8; 16], transcript));
    assert_ne!(mac, transcript_mac(&secret, b"a different transcript"));

    // the simulated man in the middle: he holds a DIFFERENT ticket, so a
    // different secret — his MAC cannot match, the joiner walks away
    let attacker_secret = [1_u8; 16];
    let forged = transcript_mac(&attacker_secret, transcript);
    assert_ne!(forged, mac);
}

// ---------------------------------------------------------------------------
// v0.6.1 (P2): the persistent-wire exchange under load — the per-frame
// streams wedged after a handful of frames under lockstep traffic; this
// pins the one-stream wire at twenty frames each way.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn the_wire_carries_many_frames_in_lockstep() {
    // a bounded test (R5): a wedged exchange fails, never hangs
    let bound = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        the_wire_carries_many_frames_in_lockstep_inner(),
    );
    bound.await.expect("il lockstep non è finito in 60 s");
}

async fn the_wire_carries_many_frames_in_lockstep_inner() {
    use engramma_sync::device::DeviceKey;
    use engramma_sync::protocol::{bind_endpoint, connect};
    let key_a = DeviceKey::from_seed([1; 32]);
    let key_b = DeviceKey::from_seed([2; 32]);
    let ep_a = bind_endpoint(&key_a, "127.0.0.1:0", &engramma_sync::relay::RelayChoice::Disabled).await.unwrap(/* t */);
    let ep_b = bind_endpoint(&key_b, "127.0.0.1:0", &engramma_sync::relay::RelayChoice::Disabled).await.unwrap(/* t */);
    let addr = ep_a.addr();
    tokio::task::LocalSet::new()
        .run_until(async {
            let server = tokio::task::spawn_local(async move {
                let Some(incoming) = ep_a.accept().await else {
                    return;
                };
                let conn = incoming.await.unwrap(/* t */);
                for i in 0..20_u64 {
                    eprintln!("S: waiting ping {i}");
                    let frame = recv_one(&conn).await;
                    eprintln!("S: got ping {i}");
                    assert_eq!(frame["kind"].as_str(), Some("ping"));
                    assert_eq!(frame["i"].as_u64(), Some(i));
                    send_one(&conn, &serde_json::json!({"kind": "pong", "i": i})).await;
                    eprintln!("S: sent pong {i}");
                }
                // the peer's goodbye: only then may we drop the
                // connection (a sooner drop resets the wire and the last
                // frame in flight vanishes)
                let bye = recv_one(&conn).await;
                assert_eq!(bye["kind"].as_str(), Some("bye"));
            });
            let conn = connect(&ep_b, addr).await.unwrap(/* t */);
            for i in 0..20_u64 {
                eprintln!("C: sending ping {i}");
                send_one(&conn, &serde_json::json!({"kind": "ping", "i": i})).await;
                eprintln!("C: sent ping {i}");
                let frame = recv_one(&conn).await;
                assert_eq!(frame["kind"].as_str(), Some("pong"));
                assert_eq!(frame["i"].as_u64(), Some(i));
            }
            // everything arrived: say goodbye so the peer may close clean
            send_one(&conn, &serde_json::json!({"kind": "bye"})).await;
            server.await.unwrap(/* t */);
        })
        .await;
}

/// one frame on one stream, with FINISH and the EOF drain — the pattern
/// the protocol uses (v0.6.1, P2: without them the transport wedges)
async fn send_one(conn: &iroh::endpoint::Connection, msg: &serde_json::Value) {
    let body = msg.to_string();
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(body.as_bytes());
    let mut send = conn.open_uni().await.unwrap(/* t */);
    send.write_all(&frame).await.unwrap(/* t */);
    send.finish().unwrap(/* t */);
}

async fn recv_one(conn: &iroh::endpoint::Connection) -> serde_json::Value {
    let mut recv = conn.accept_uni().await.unwrap(/* t */);
    let mut len_buf = [0_u8; 4];
    recv.read_exact(&mut len_buf).await.unwrap(/* t */);
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0_u8; len];
    recv.read_exact(&mut buf).await.unwrap(/* t */);
    let _ = recv.read_to_end(1).await;
    serde_json::from_slice(&buf).unwrap(/* t */)
}
