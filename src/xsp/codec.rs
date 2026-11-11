//! Encodages du protocole xs-parley (SPEC xs-parley §3, §5) : base64url strict, JSON canonique
//! (RFC 8785), horodatages au profil `AAAA-MM-JJTHH:MM:SS.sssZ`, identifiants UUID v7 et formes
//! des noms, empreintes et sessions.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::Value;

pub fn b64u_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// base64url sans remplissage ; des bits de queue non canoniques sont refusés, comme une longueur
/// différente de `expected`.
pub fn b64u_decode(s: &str, expected: Option<usize>) -> Result<Vec<u8>, &'static str> {
    let bytes = URL_SAFE_NO_PAD.decode(s).map_err(|_| "bad_encoding")?;
    if expected.is_some_and(|n| bytes.len() != n) {
        return Err("bad_encoding");
    }
    Ok(bytes)
}

/// Décode exactement `N` octets.
pub fn b64u_array<const N: usize>(s: &str) -> Result<[u8; N], &'static str> {
    let bytes = b64u_decode(s, Some(N))?;
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// JSON canonique RFC 8785.
pub fn jcs(value: &Value) -> String {
    // Une `Value` ne contient que des nombres finis : la canonicalisation ne peut pas échouer.
    serde_jcs::to_string(value).expect("JCS d'une valeur JSON")
}

const DAY_MS: u64 = 86_400_000;

/// Jours depuis le 1970-01-01 du jour civil `(y, m, d)` (algorithme days_from_civil de H. Hinnant).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Jour civil `(y, m, d)` des `z` jours depuis le 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `AAAA-MM-JJTHH:MM:SS.sssZ`, comme `Date.toISOString()`.
pub fn format_timestamp(ms: u64) -> String {
    let (y, mo, d) = civil_from_days((ms / DAY_MS) as i64);
    let rest = ms % DAY_MS;
    let (h, mi, s, milli) = (rest / 3_600_000, rest / 60_000 % 60, rest / 1000 % 60, rest % 1000);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

/// Inverse de `format_timestamp` ; une forme ou une date invalide (2026-02-30, 24:00…) → `None`.
pub fn parse_timestamp(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let shape = b"0000-00-00T00:00:00.000Z";
    if b.len() != shape.len() {
        return None;
    }
    for (&c, &want) in b.iter().zip(shape) {
        if (want == b'0' && !c.is_ascii_digit()) || (want != b'0' && c != want) {
            return None;
        }
    }
    let num = |from: usize, to: usize| s[from..to].parse::<u32>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec, milli) = (num(11, 13)?, num(14, 16)?, num(17, 19)?, num(20, 23)?);
    if !(1..=12).contains(&mo) || d == 0 || d > days_in_month(i64::from(y), mo) || h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    let days = u64::try_from(days_from_civil(i64::from(y), mo, d)).ok()?;
    Some(days * DAY_MS + u64::from(h) * 3_600_000 + u64::from(mi) * 60_000 + u64::from(sec) * 1000 + u64::from(milli))
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("générateur aléatoire du système");
    out
}

/// UUID v7 (RFC 9562) comme `newMsgId` : 48 bits de millisecondes, version 7, variante 10.
pub fn new_msg_id() -> String {
    let mut b = random_bytes::<16>();
    b[..6].copy_from_slice(&now_ms().to_be_bytes()[2..]);
    b[6] = (b[6] & 0x0f) | 0x70;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

fn is_b64u_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

/// UUID en minuscules (`msg_id`, `reply_to`, id de session omp, SPEC xs-parley §1).
pub fn is_session_id(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_digit() || (b'a'..=b'f').contains(&c),
        })
}

/// `xs:` + 43 caractères base64url.
pub fn is_fp(s: &str) -> bool {
    s.len() == 46 && s.starts_with("xs:") && s.bytes().skip(3).all(is_b64u_char)
}

/// Alias ou nom de parley : 1 à 32 caractères `[A-Za-z0-9_-]`.
pub fn is_name(s: &str) -> bool {
    (1..=32).contains(&s.len()) && s.bytes().all(is_b64u_char)
}

/// `parley_id` : 22 caractères base64url (16 octets).
pub fn is_parley_id(s: &str) -> bool {
    s.len() == 22 && s.bytes().all(is_b64u_char)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_trip_and_reject_impossible_dates() {
        assert_eq!(parse_timestamp("2026-11-01T12:00:01.000Z"), Some(1_793_534_401_000));
        assert_eq!(format_timestamp(1_793_534_401_000), "2026-11-01T12:00:01.000Z");
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(parse_timestamp("2024-02-29T23:59:59.999Z").map(format_timestamp).as_deref(), Some("2024-02-29T23:59:59.999Z"));
        for bad in ["2026-02-29T00:00:00.000Z", "2026-13-01T00:00:00.000Z", "2026-11-01T24:00:00.000Z", "2026-11-01T12:00:01Z", "2026-11-01 12:00:01.000Z"] {
            assert_eq!(parse_timestamp(bad), None, "{bad}");
        }
    }

    #[test]
    fn msg_ids_are_uuid_v7() {
        let id = new_msg_id();
        assert!(is_session_id(&id), "{id}");
        assert_eq!(&id[14..15], "7");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        let ms = u64::from_str_radix(&id[..8], 16).unwrap() << 16 | u64::from_str_radix(&id[9..13], 16).unwrap();
        assert!(now_ms().abs_diff(ms) < 5_000);
    }

    #[test]
    fn b64u_rejects_padding_and_non_canonical_tail_bits() {
        assert_eq!(b64u_decode("AQ", None), Ok(vec![1]));
        assert!(b64u_decode("AR", None).is_err());
        assert!(b64u_decode("AQ==", None).is_err());
        assert!(b64u_decode("AQ", Some(2)).is_err());
    }
}
