//! Certificats de membre (SPEC xs-parley §4.2) : `{body, sig}`, `sig = Ed25519(graine du rang,
//! JCS(body))`, et boîte `cert_box` chiffrée par `K_grp` pour le relai.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::XChaCha20Poly1305;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::codec::{
    b64u_array, b64u_encode, format_timestamp, is_name, is_session_id, jcs, parse_timestamp, random_bytes,
};
use super::keys::{fingerprint, rank_pubs, ParleyKey};

/// Rang des certificats du session-manager.
pub const MANAGER_RANK: &str = "M";
const CERT_MAX_AGE_MS: u64 = 25 * 3_600_000;
const CERT_MAX_FUTURE_MS: u64 = 5 * 60_000;
const ROLE_MAX: usize = 280;
const BODY_KEYS: [&str; 9] = ["v", "parley_id", "rank", "sign_pub", "kem_pub", "alias", "role", "session", "issued_at"];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertBody {
    pub v: u8,
    pub parley_id: String,
    pub rank: String,
    pub sign_pub: String,
    pub kem_pub: String,
    pub alias: String,
    pub role: String,
    pub session: String,
    pub issued_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cert {
    pub body: CertBody,
    pub sig: String,
}

impl Cert {
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("certificat sérialisable")
    }

    /// Clé publique X25519 annoncée, déjà validée par `verify_cert`.
    pub fn kem_pub(&self) -> Result<[u8; 32], &'static str> {
        b64u_array(&self.body.kem_pub)
    }
}

/// Rôle valide : pas de caractère de contrôle (ni U+2028/U+2029), 280 points de code au plus.
fn is_role(role: &str) -> bool {
    role.chars().count() <= ROLE_MAX && !role.chars().any(|c| c.is_control() || c == '\u{2028}' || c == '\u{2029}')
}

fn sign(seed: &[u8; 32], message: &[u8]) -> String {
    b64u_encode(&SigningKey::from_bytes(seed).sign(message).to_bytes())
}

/// Vérification stricte RFC 8032 (S < L, encodages canoniques, pas de point de petit ordre).
pub fn verify_strict(public: &[u8; 32], message: &[u8], sig: &[u8; 64]) -> bool {
    VerifyingKey::from_bytes(public).is_ok_and(|key| key.verify_strict(message, &Signature::from_bytes(sig)).is_ok())
}

/// Certificat de rang `M` signé par la graine answer de `key` (rang `S`).
#[allow(clippy::too_many_arguments)]
pub fn sign_manager_cert(
    key: &ParleyKey,
    sign_pub: &[u8; 32],
    kem_pub: &[u8; 32],
    alias: &str,
    role: &str,
    session: &str,
    now_ms: u64,
) -> Cert {
    let body = CertBody {
        v: 1,
        parley_id: key.parley_id.clone(),
        rank: MANAGER_RANK.into(),
        sign_pub: b64u_encode(sign_pub),
        kem_pub: b64u_encode(kem_pub),
        alias: alias.into(),
        role: role.into(),
        session: session.into(),
        issued_at: format_timestamp(now_ms),
    };
    let message = jcs(&serde_json::to_value(&body).expect("corps sérialisable"));
    Cert { sig: sign(&key.seed, message.as_bytes()), body }
}

/// Validation §4.2, étapes 1 à 6 dans l'ordre ; la première erreur l'emporte. Codes d'erreur de
/// `CryptoErrorCode`.
pub fn verify_cert(input: &Value, key: &ParleyKey, parley_id: &str, fp_announced: &str, now_ms: u64) -> Result<Cert, &'static str> {
    // 1. Structure.
    let Some(outer) = input.as_object().filter(|o| o.len() == 2 && o.contains_key("body") && o.contains_key("sig")) else {
        return Err("bad_structure");
    };
    let Some(body) = outer["body"].as_object().filter(|b| b.len() == BODY_KEYS.len() && BODY_KEYS.iter().all(|k| b.contains_key(*k)))
    else {
        return Err("bad_structure");
    };
    if body["v"].as_u64() != Some(1) {
        return Err("bad_structure");
    }
    if !matches!(body["rank"].as_str(), Some("A" | "S" | MANAGER_RANK)) {
        return Err("bad_structure");
    }
    if BODY_KEYS[1..].iter().any(|k| !body[*k].is_string()) {
        return Err("bad_structure");
    }
    let Some(sig) = outer["sig"].as_str() else { return Err("bad_structure") };
    let body: CertBody = serde_json::from_value(Value::Object(body.clone())).map_err(|_| "bad_structure")?;

    // 2. Encodages.
    let sign_pub = b64u_array::<32>(&body.sign_pub)?;
    b64u_array::<32>(&body.kem_pub)?;
    let sig_bytes = b64u_array::<64>(sig)?;
    if !is_name(&body.alias) || !is_role(&body.role) || !is_session_id(&body.session) {
        return Err("bad_structure");
    }
    let issued = parse_timestamp(&body.issued_at).ok_or("bad_timestamp")?;

    // 3. Fraîcheur.
    if issued + CERT_MAX_AGE_MS < now_ms || issued > now_ms + CERT_MAX_FUTURE_MS {
        return Err("stale_cert");
    }

    // 4. Parley : la clé locale n'appartient qu'à une parley.
    if body.parley_id != parley_id || key.parley_id != parley_id {
        return Err("wrong_parley");
    }

    // 5. Signature par la clé du rang, dérivée de la clé locale ; `M` signe avec la graine answer.
    let (pub_ask, pub_answer) = rank_pubs(key);
    let rank_pub = if body.rank == "A" { pub_ask } else { pub_answer };
    let message = jcs(&serde_json::to_value(&body).expect("corps sérialisable"));
    if !verify_strict(&rank_pub, message.as_bytes(), &sig_bytes) {
        return Err("bad_signature");
    }

    // 6. Empreinte.
    if fingerprint(&sign_pub) != fp_announced {
        return Err("fp_mismatch");
    }
    Ok(Cert { body, sig: sig.into() })
}

/// `b64u(nonce ‖ XChaCha20-Poly1305(K_grp, nonce, JCS(cert), aad = parley_id))` ; point d'entrée
/// des vecteurs de test.
pub fn seal_cert_box_with_nonce(k_grp: &[u8; 32], parley_id: &str, cert: &Cert, nonce: &[u8; 24]) -> String {
    let clear = jcs(&cert.to_value());
    let cipher = XChaCha20Poly1305::new(k_grp.into());
    let ct = cipher
        .encrypt(nonce.into(), Payload { msg: clear.as_bytes(), aad: parley_id.as_bytes() })
        .expect("chiffrement XChaCha20-Poly1305");
    b64u_encode(&[nonce.as_slice(), &ct].concat())
}

pub fn seal_cert_box(k_grp: &[u8; 32], parley_id: &str, cert: &Cert) -> String {
    seal_cert_box_with_nonce(k_grp, parley_id, cert, &random_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::xsp::keys::{group_key, parse};

    #[test]
    fn certificates_match_the_vectors_and_fail_with_the_expected_codes() {
        let v: Value = serde_json::from_str(include_str!("vectors/cert.json")).unwrap();
        let now = parse_timestamp(v["now"].as_str().unwrap()).unwrap();
        for case in v["cases"].as_array().unwrap() {
            let key = parse(case["signing_key"].as_str().unwrap()).unwrap();
            let parley_id = case["parley_id"].as_str().unwrap();
            let fp = case["fp"].as_str().unwrap();
            let cert = &case["cert"];
            assert_eq!(jcs(&cert["body"]), case["jcs_body"]);
            assert_eq!(sign(&key.seed, case["jcs_body"].as_str().unwrap().as_bytes()), cert["sig"]);
            let ok = verify_cert(cert, &key, parley_id, fp, now).unwrap();
            assert_eq!(ok.to_value(), *cert);

            let mut other_rank = cert.clone();
            other_rank["body"]["rank"] = if cert["body"]["rank"] == "A" { "S" } else { "A" }.into();
            assert_eq!(verify_cert(&other_rank, &key, parley_id, fp, now), Err("bad_signature"));
            let other_fp = format!("xs:{}", "A".repeat(43));
            assert_eq!(verify_cert(cert, &key, parley_id, &other_fp, now), Err("fp_mismatch"));
            assert_eq!(verify_cert(cert, &key, parley_id, fp, now + 26 * 3_600_000), Err("stale_cert"));
            let mut extra = cert.clone();
            extra["body"]["extra"] = 1.into();
            assert_eq!(verify_cert(&extra, &key, parley_id, fp, now), Err("bad_structure"));
        }
    }

    #[test]
    fn a_manager_certificate_verifies_against_the_answer_key_only() {
        let v: Value = serde_json::from_str(include_str!("vectors/k-grp.json")).unwrap();
        let ask = parse(v["key_ask"].as_str().unwrap()).unwrap();
        let answer = parse(v["key_answer"].as_str().unwrap()).unwrap();
        let seed = [7u8; 32];
        let public = crate::xsp::keys::sign_pub(&seed);
        let fp = fingerprint(&public);
        let now = 1_793_534_401_000;
        let cert = sign_manager_cert(&answer, &public, &[9u8; 32], "xst-mac", "session-manager xst (mac)", "019a5f3c-1b2d-7e4f-8a6b-1c2d3e4f5a6b", now);
        assert_eq!(cert.body.rank, "M");
        // Un master comme une session vérifie un certificat M avec la clé publique answer.
        assert!(verify_cert(&cert.to_value(), &ask, &ask.parley_id, &fp, now).is_ok());
        assert!(verify_cert(&cert.to_value(), &answer, &answer.parley_id, &fp, now).is_ok());
        // Signé par la graine ask, un certificat M est refusé.
        let forged = sign_manager_cert(&ask, &public, &[9u8; 32], "xst-mac", "r", "019a5f3c-1b2d-7e4f-8a6b-1c2d3e4f5a6b", now);
        assert_eq!(verify_cert(&forged.to_value(), &ask, &ask.parley_id, &fp, now), Err("bad_signature"));
    }

    #[test]
    fn cert_box_matches_the_vector() {
        let v: Value = serde_json::from_str(include_str!("vectors/cert-box.json")).unwrap();
        let k_grp = b64u_array::<32>(v["k_grp"].as_str().unwrap()).unwrap();
        let nonce = b64u_array::<24>(v["nonce"].as_str().unwrap()).unwrap();
        let cert: Cert = serde_json::from_value(v["cert"].clone()).unwrap();
        assert_eq!(seal_cert_box_with_nonce(&k_grp, v["parley_id"].as_str().unwrap(), &cert, &nonce), v["cert_box"]);
        let g: Value = serde_json::from_str(include_str!("vectors/k-grp.json")).unwrap();
        assert_eq!(group_key(&parse(g["key_ask"].as_str().unwrap()).unwrap()), k_grp);
    }
}
