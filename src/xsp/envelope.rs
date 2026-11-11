//! Enveloppes de bout en bout (SPEC xs-parley §5) : HPKE base mode X25519-HKDF-SHA256 /
//! HKDF-SHA256 / ChaCha20-Poly1305, `info = "xs-parley env v1"`, `aad = JCS(h)` ; signature
//! Ed25519 de `JCS({h, enc, ct})` par la clé de signature de l'émetteur.

use ed25519_dalek::{Signer, SigningKey};
use hpke::aead::ChaCha20Poly1305;
use hpke::kdf::HkdfSha256;
use hpke::kem::X25519HkdfSha256;
use hpke::{Deserializable, Kem, OpModeR, OpModeS, Serializable};
use serde::Serialize;
use serde_json::{Map, Value, json};

use super::cert::{verify_cert, verify_strict, Cert};
use super::codec::{b64u_array, b64u_decode, b64u_encode, is_fp, is_session_id, jcs, parse_timestamp, random_bytes};
use super::keys::ParleyKey;

type KemSuite = X25519HkdfSha256;
const HPKE_INFO: &[u8] = b"xs-parley env v1";
/// Taille maximale de `JCS(plain)`.
pub const PLAIN_MAX: usize = 47_104;
const TS_SKEW_MS: u64 = 5 * 60_000;
const TAG_LEN: usize = 16;
const KINDS: [&str; 7] = ["request", "accepted", "response", "question", "answer", "destroy", "spawn"];
const STATUSES: [&str; 5] = ["ok", "refused", "aborted", "error", "busy"];
const HEADER_KEYS: [&str; 8] = ["v", "parley_id", "msg_id", "kind", "from", "to", "reply_to", "ts"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Header {
    pub v: u8,
    pub parley_id: String,
    pub msg_id: String,
    pub kind: String,
    pub from: String,
    pub to: String,
    pub reply_to: Option<String>,
    pub ts: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Plain {
    pub cert: Cert,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Paire X25519 éphémère `(privée, publique)` tirée d'un IKM aléatoire de 32 octets.
pub fn kem_pair() -> ([u8; 32], [u8; 32]) {
    let (private, public) = KemSuite::derive_keypair(&random_bytes::<32>());
    (private.to_bytes().into(), public.to_bytes().into())
}

fn sig_input(h: &Value, enc: &str, ct: &str) -> String {
    jcs(&json!({ "h": h, "enc": enc, "ct": ct }))
}

/// Chiffre `plain` pour `recipient_kem_pub` et signe avec `sign_seed`. Une `reason` trop longue est
/// tronquée pour que `JCS(plain)` tienne dans `PLAIN_MAX` ; un texte trop long est refusé.
pub fn seal_envelope(h: &Header, plain: &Plain, recipient_kem_pub: &[u8; 32], sign_seed: &[u8; 32]) -> Result<Value, &'static str> {
    let mut plain = plain.clone();
    let size = |p: &Plain| jcs(&serde_json::to_value(p).expect("clair sérialisable")).len();
    while size(&plain) > PLAIN_MAX {
        match plain.reason.as_mut() {
            Some(reason) if !reason.is_empty() => {
                let keep = reason.chars().count() / 2;
                *reason = reason.chars().take(keep).collect();
            }
            _ => return Err("bad_structure"),
        }
    }
    let h_value = serde_json::to_value(h).expect("en-tête sérialisable");
    let aad = jcs(&h_value);
    let clear = jcs(&serde_json::to_value(&plain).expect("clair sérialisable"));
    let public = <KemSuite as Kem>::PublicKey::from_bytes(recipient_kem_pub).map_err(|_| "bad_key")?;
    let (enc, ct) = hpke::single_shot_seal::<ChaCha20Poly1305, HkdfSha256, KemSuite>(
        &OpModeS::Base,
        &public,
        HPKE_INFO,
        clear.as_bytes(),
        aad.as_bytes(),
    )
    .map_err(|_| "bad_key")?;
    let enc = b64u_encode(&enc.to_bytes());
    let ct = b64u_encode(&ct);
    let sig = SigningKey::from_bytes(sign_seed).sign(sig_input(&h_value, &enc, &ct).as_bytes());
    Ok(json!({ "h": h_value, "enc": enc, "ct": ct, "sig": b64u_encode(&sig.to_bytes()) }))
}

fn parse_header(h: &Map<String, Value>) -> Result<Header, &'static str> {
    if h.len() != HEADER_KEYS.len() || !HEADER_KEYS.iter().all(|k| h.contains_key(*k)) {
        return Err("bad_structure");
    }
    let text = |k: &str| h[k].as_str().ok_or("bad_structure");
    if h["v"].as_u64() != Some(1) {
        return Err("bad_structure");
    }
    let (parley_id, msg_id, kind, from, to, ts) = (text("parley_id")?, text("msg_id")?, text("kind")?, text("from")?, text("to")?, text("ts")?);
    if !is_session_id(msg_id) || !KINDS.contains(&kind) || !is_fp(from) || !is_fp(to) {
        return Err("bad_structure");
    }
    let reply_to = match &h["reply_to"] {
        Value::Null => None,
        Value::String(s) if is_session_id(s) => Some(s.clone()),
        _ => return Err("bad_structure"),
    };
    Ok(Header {
        v: 1,
        parley_id: parley_id.into(),
        msg_id: msg_id.into(),
        kind: kind.into(),
        from: from.into(),
        to: to.into(),
        reply_to,
        ts: ts.into(),
    })
}

/// Clair déchiffré : `cert` brut (validé ensuite), texte, statut et raison.
type RawPlain = (Value, String, Option<String>, Option<String>);

fn parse_plain(clear: &[u8], kind: &str) -> Result<RawPlain, &'static str> {
    let text = std::str::from_utf8(clear).map_err(|_| "bad_structure")?;
    let Ok(Value::Object(mut value)) = serde_json::from_str::<Value>(text) else { return Err("bad_structure") };
    let allowed: &[&str] = if matches!(kind, "accepted" | "response") { &["cert", "text", "status", "reason"] } else { &["cert", "text"] };
    if !value.keys().all(|k| allowed.contains(&k.as_str())) {
        return Err("bad_structure");
    }
    let cert = value.remove("cert").ok_or("bad_structure")?;
    let Some(Value::String(body)) = value.remove("text") else { return Err("bad_structure") };
    let status = match value.remove("status") {
        None => None,
        Some(Value::String(s)) if STATUSES.contains(&s.as_str()) => Some(s),
        Some(_) => return Err("bad_structure"),
    };
    let reason = match value.remove("reason") {
        None => None,
        Some(Value::String(s)) => Some(s),
        Some(_) => return Err("bad_structure"),
    };
    Ok((cert, body, status, reason))
}

/// §5.2, étapes 1 à 5 (le cache anti-rejeu est à l'appelant). `kem_privs` : clé courante puis
/// précédente.
pub fn open_envelope(
    input: &Value,
    parley_id: &str,
    self_fp: &str,
    key: &ParleyKey,
    kem_privs: &[[u8; 32]],
    now_ms: u64,
) -> Result<(Header, Plain, Cert), &'static str> {
    // 1. Adressage, puis structure stricte.
    let Some(env) = input.as_object() else { return Err("bad_structure") };
    let Some(h_raw) = env.get("h").and_then(Value::as_object) else { return Err("bad_structure") };
    if h_raw.get("parley_id").and_then(Value::as_str) != Some(parley_id) || key.parley_id != parley_id {
        return Err("bad_address");
    }
    if h_raw.get("to").and_then(Value::as_str) != Some(self_fp) {
        return Err("bad_address");
    }
    if env.len() != 4 || !["h", "enc", "ct", "sig"].iter().all(|k| env.contains_key(*k)) {
        return Err("bad_structure");
    }
    let (Some(enc), Some(ct), Some(sig)) = (env["enc"].as_str(), env["ct"].as_str(), env["sig"].as_str()) else {
        return Err("bad_structure");
    };
    let h = parse_header(h_raw)?;

    // 2. Déchiffrement HPKE : clé courante, puis précédente.
    let enc_bytes = b64u_array::<32>(enc)?;
    let ct_bytes = b64u_decode(ct, None)?;
    if ct_bytes.len() < TAG_LEN {
        return Err("bad_encoding");
    }
    let aad = jcs(&env["h"]);
    let encapped = <KemSuite as Kem>::EncappedKey::from_bytes(&enc_bytes).map_err(|_| "decrypt_failed")?;
    let clear = kem_privs
        .iter()
        .find_map(|private| {
            let private = <KemSuite as Kem>::PrivateKey::from_bytes(private).ok()?;
            hpke::single_shot_open::<ChaCha20Poly1305, HkdfSha256, KemSuite>(
                &OpModeR::Base,
                &private,
                &encapped,
                HPKE_INFO,
                &ct_bytes,
                aad.as_bytes(),
            )
            .ok()
        })
        .ok_or("decrypt_failed")?;
    let (raw_cert, text, status, reason) = parse_plain(&clear, &h.kind)?;

    // 3. Certificat de l'émetteur, pour la parley et l'empreinte de l'en-tête.
    let cert = verify_cert(&raw_cert, key, &h.parley_id, &h.from, now_ms)?;

    // 4. Signature de l'enveloppe par la clé de signature du certificat.
    let sig_bytes = b64u_array::<64>(sig)?;
    let sign_pub = b64u_array::<32>(&cert.body.sign_pub)?;
    if !verify_strict(&sign_pub, sig_input(&env["h"], enc, ct).as_bytes(), &sig_bytes) {
        return Err("bad_signature");
    }

    // 5. Horodatage.
    let ts = parse_timestamp(&h.ts).ok_or("bad_timestamp")?;
    if ts.abs_diff(now_ms) > TS_SKEW_MS {
        return Err("bad_timestamp");
    }
    Ok((h, Plain { cert: cert.clone(), text, status, reason }, cert))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xsp::codec::format_timestamp;
    use crate::xsp::keys::parse;

    fn vector() -> Value {
        serde_json::from_str(include_str!("vectors/envelope.json")).unwrap()
    }

    #[test]
    fn the_vector_envelope_opens_to_the_expected_plain() {
        let v = vector();
        let key = parse(v["verifier_key"].as_str().unwrap()).unwrap();
        let now = parse_timestamp(v["now"].as_str().unwrap()).unwrap();
        let private = b64u_array::<32>(v["recipient_kem_priv"].as_str().unwrap()).unwrap();
        let to = v["h"]["to"].as_str().unwrap();
        assert_eq!(jcs(&v["h"]), v["jcs_h"]);
        let (h, plain, _) = open_envelope(&v["envelope"], v["parley_id"].as_str().unwrap(), to, &key, &[[1u8; 32], private], now).unwrap();
        assert_eq!(h.kind, "request");
        assert_eq!(plain.text, v["plain"]["text"]);
        assert_eq!(serde_json::to_value(&plain).unwrap(), v["plain"]);
        assert_eq!(jcs(&v["plain"]), v["jcs_plain"]);

        let late = now + 6 * 60_000;
        assert_eq!(open_envelope(&v["envelope"], v["parley_id"].as_str().unwrap(), to, &key, &[private], late), Err("bad_timestamp"));
        assert_eq!(open_envelope(&v["envelope"], v["parley_id"].as_str().unwrap(), to, &key, &[[1u8; 32]], now), Err("decrypt_failed"));
        let other = format!("xs:{}", "B".repeat(43));
        assert_eq!(open_envelope(&v["envelope"], v["parley_id"].as_str().unwrap(), &other, &key, &[private], now), Err("bad_address"));
    }

    #[test]
    fn a_sealed_envelope_round_trips_and_rejects_tampering() {
        let v = vector();
        let key = parse(v["verifier_key"].as_str().unwrap()).unwrap();
        let now = parse_timestamp(v["now"].as_str().unwrap()).unwrap();
        let sender_seed = b64u_array::<32>(v["sender_sign_seed"].as_str().unwrap()).unwrap();
        let cert: Cert = serde_json::from_value(v["plain"]["cert"].clone()).unwrap();
        let (private, public) = kem_pair();
        let h = Header {
            v: 1,
            parley_id: v["parley_id"].as_str().unwrap().into(),
            msg_id: crate::xsp::codec::new_msg_id(),
            kind: "response".into(),
            from: v["h"]["from"].as_str().unwrap().into(),
            to: v["h"]["to"].as_str().unwrap().into(),
            reply_to: Some("01939f0c-7a3e-7b2d-8c4e-5f6a7b8c9d0e".into()),
            ts: format_timestamp(now),
        };
        let plain = Plain { cert, text: String::new(), status: Some("ok".into()), reason: Some("déjà ouverte".into()) };
        let env = seal_envelope(&h, &plain, &public, &sender_seed).unwrap();
        let (opened_h, opened, _) = open_envelope(&env, &h.parley_id, &h.to, &key, &[private], now).unwrap();
        assert_eq!(opened_h, h);
        assert_eq!(opened, plain);

        let mut tampered = env.clone();
        tampered["h"]["ts"] = format_timestamp(now - 1).into();
        assert_eq!(open_envelope(&tampered, &h.parley_id, &h.to, &key, &[private], now), Err("decrypt_failed"));
        let mut old = h.clone();
        old.ts = format_timestamp(now - 6 * 60_000);
        let env = seal_envelope(&old, &plain, &public, &sender_seed).unwrap();
        assert_eq!(open_envelope(&env, &h.parley_id, &h.to, &key, &[private], now), Err("bad_timestamp"));
        // Un `spawn` ne porte ni statut ni raison.
        let spawn = Header { kind: "spawn".into(), ..h.clone() };
        let env = seal_envelope(&spawn, &plain, &public, &sender_seed).unwrap();
        assert_eq!(open_envelope(&env, &h.parley_id, &h.to, &key, &[private], now), Err("bad_structure"));
    }
}
