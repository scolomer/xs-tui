//! Clés de parley (SPEC xs-parley §4.1) : `xs2:<relai>:<parley_id>:<nom>:<rang>:<graine>:<autre>`,
//! `autre` = graine answer pour le rang `A`, clé publique ask pour le rang `S` ; clé de groupe
//! `K_grp` et empreintes (§3).

use ed25519_dalek::{SigningKey, VerifyingKey};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

use super::codec::{b64u_array, b64u_decode, b64u_encode, is_name};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyRank {
    A,
    S,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParleyKey {
    pub relay: String,
    pub parley_id: String,
    pub name: String,
    pub rank: KeyRank,
    /// Graine Ed25519 du rang de la clé.
    pub seed: [u8; 32],
    /// Rang `A` : graine answer ; rang `S` : clé publique ask.
    pub other: [u8; 32],
}

const KEY_VERSION: &str = "xs2";

/// Relai `hôte[:port]` ou `[ipv6][:port]`, comme `RELAY_RE`.
fn is_relay(relay: &str) -> bool {
    let (host, port) = match relay.rfind(':') {
        Some(i) if !relay.ends_with(']') && (!relay.starts_with('[') || relay[..i].ends_with(']')) => {
            (&relay[..i], Some(&relay[i + 1..]))
        }
        _ => (relay, None),
    };
    if port.is_some_and(|p| p.is_empty() || p.len() > 5 || !p.bytes().all(|c| c.is_ascii_digit())) {
        return false;
    }
    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        return !inner.is_empty() && inner.bytes().all(|c| c.is_ascii_hexdigit() || c == b':' || c == b'.');
    }
    let b = host.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && b.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-')
}

/// Même découpage que `parseParleyKey` : les cinq derniers champs depuis la droite, le relai
/// pouvant contenir un port.
pub fn parse(text: &str) -> Result<ParleyKey, String> {
    let s = text.trim();
    if s.starts_with("xs1:") {
        return Err("clé de parley d'un format obsolète (xs1) : le créateur doit recréer la parley".into());
    }
    let Some(body) = s.strip_prefix("xs2:") else {
        return Err(format!("clé de parley invalide : préfixe {KEY_VERSION}: attendu"));
    };
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() < 6 {
        return Err("clé de parley invalide : champs manquants".into());
    }
    let (relay, tail) = parts.split_at(parts.len() - 5);
    let relay = relay.join(":");
    let [parley_id, name, rank, seed, other] = [tail[0], tail[1], tail[2], tail[3], tail[4]];
    if !is_relay(&relay) {
        return Err("clé de parley invalide : relai".into());
    }
    if !is_name(name) {
        return Err("clé de parley invalide : nom".into());
    }
    let rank = match rank {
        "A" => KeyRank::A,
        "S" => KeyRank::S,
        _ => return Err("clé de parley invalide : rang".into()),
    };
    let encoding = |_| "clé de parley invalide : encodage".to_string();
    b64u_decode(parley_id, Some(16)).map_err(encoding)?;
    let seed = b64u_array::<32>(seed).map_err(encoding)?;
    let other = b64u_array::<32>(other).map_err(encoding)?;
    if rank == KeyRank::S && VerifyingKey::from_bytes(&other).is_err() {
        return Err("clé de parley invalide : clé publique".into());
    }
    Ok(ParleyKey { relay, parley_id: parley_id.into(), name: name.into(), rank, seed, other })
}

#[cfg(test)]
pub fn format(key: &ParleyKey) -> String {
    let rank = match key.rank {
        KeyRank::A => "A",
        KeyRank::S => "S",
    };
    [KEY_VERSION, &key.relay, &key.parley_id, &key.name, rank, &b64u_encode(&key.seed), &b64u_encode(&key.other)].join(":")
}

pub fn sign_pub(seed: &[u8; 32]) -> [u8; 32] {
    SigningKey::from_bytes(seed).verifying_key().to_bytes()
}

/// Clé answer (rang `S`) d'une parley, dérivée d'une clé ask ; une clé `S` est rendue telle quelle.
pub fn answer_key(key: &ParleyKey) -> ParleyKey {
    match key.rank {
        KeyRank::S => key.clone(),
        KeyRank::A => ParleyKey { rank: KeyRank::S, seed: key.other, other: sign_pub(&key.seed), ..key.clone() },
    }
}

/// `(pub_ask, pub_answer)`.
pub fn rank_pubs(key: &ParleyKey) -> ([u8; 32], [u8; 32]) {
    match key.rank {
        KeyRank::A => (sign_pub(&key.seed), sign_pub(&key.other)),
        KeyRank::S => (key.other, sign_pub(&key.seed)),
    }
}

/// `K_grp = HKDF-SHA256(pub_ask ‖ pub_answer, salt vide, "xs-parley cert v1" ‖ parley_id, 32)`.
pub fn group_key(key: &ParleyKey) -> [u8; 32] {
    let (ask, answer) = rank_pubs(key);
    let ikm = [ask, answer].concat();
    let info = [b"xs-parley cert v1".as_slice(), key.parley_id.as_bytes()].concat();
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(None, &ikm).expand(&info, &mut out).expect("32 octets de HKDF");
    out
}

/// `xs:` + base64url(SHA-256(clé publique Ed25519)).
pub fn fingerprint(sign_pub: &[u8; 32]) -> String {
    format!("xs:{}", b64u_encode(&Sha256::digest(sign_pub)))
}

/// URL WebSocket du relai : `wss://<relai>/ws`, `ws://` pour une parley rejointe avec `--notls`.
pub fn endpoint(relay: &str, notls: bool) -> String {
    format!("{}://{relay}/ws", if notls { "ws" } else { "wss" })
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn vector(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn fingerprints_match_the_vectors() {
        let v = vector(include_str!("vectors/fingerprint.json"));
        for case in v["cases"].as_array().unwrap() {
            let seed = b64u_array::<32>(case["sign_seed"].as_str().unwrap()).unwrap();
            let public = sign_pub(&seed);
            assert_eq!(b64u_encode(&public), case["sign_pub"]);
            assert_eq!(fingerprint(&public), case["fp"]);
        }
    }

    #[test]
    fn parley_keys_parse_like_the_vectors_and_derive_the_answer_key() {
        let v = vector(include_str!("vectors/parley-key.json"));
        for case in v["cases"].as_array().unwrap() {
            let key = parse(case["key"].as_str().unwrap()).unwrap();
            assert_eq!(key.relay, case["relay"]);
            assert_eq!(key.parley_id, case["parley_id"]);
            assert_eq!(key.name, case["name"]);
            assert_eq!(if key.rank == KeyRank::A { "A" } else { "S" }, case["rank"]);
            assert_eq!(b64u_encode(&key.seed), case["seed"]);
            assert_eq!(b64u_encode(&key.other), case["other"]);
            assert_eq!(format(&key), case["key"]);
        }
        let g = vector(include_str!("vectors/k-grp.json"));
        let ask = parse(g["key_ask"].as_str().unwrap()).unwrap();
        assert_eq!(format(&answer_key(&ask)), g["key_answer"]);
        assert!(parse("xs1:relay:CZvT9oQ5Qyyla-4NhysqNg:n:A:a:b").unwrap_err().contains("xs1"));
    }

    #[test]
    fn both_ranks_derive_the_same_group_key() {
        let g = vector(include_str!("vectors/k-grp.json"));
        let ask = parse(g["key_ask"].as_str().unwrap()).unwrap();
        let answer = parse(g["key_answer"].as_str().unwrap()).unwrap();
        let (pub_ask, pub_answer) = rank_pubs(&ask);
        assert_eq!(b64u_encode(&pub_ask), g["pub_ask"]);
        assert_eq!(b64u_encode(&pub_answer), g["pub_answer"]);
        assert_eq!(b64u_encode(&group_key(&ask)), g["k_grp"]);
        assert_eq!(group_key(&answer), group_key(&ask));
    }
}
