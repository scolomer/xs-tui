//! Fichier publié par l'extension agentz pour un agent : `~/.omp/run/agentz/<pid>/<agentId>.json`.
//!
//! Le format est décrit dans SPEC.md § Format ; il évolue dans le même commit que `extension/`.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Version du format comprise par ce binaire.
pub const FORMAT_VERSION: u32 = 1;

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
    Working,
    Blocked,
    Error,
}

#[derive(Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Main,
    Sub,
}

/// Champs lus par agz ; les autres (`depth`, `updatedAt`…) sont ignorés.
#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub kind: Kind,
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
}

#[derive(Deserialize, Clone, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Zellij {
    pub session: String,
    pub pane_id: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub pid: i32,
    pub agent: Agent,
    pub session_id: String,
    pub cwd: PathBuf,
    pub title: String,
    pub state: State,
    pub zellij: Option<Zellij>,
    pub started_at: String,
}

#[expect(clippy::large_enum_variant, reason = "valeur transitoire, aussitôt déplacée dans le modèle")]
pub enum Parsed {
    Ok(Record),
    /// Fichier illisible ou de version inconnue : affiché comme ligne d'erreur à la place du pid.
    Error { pid: i32, message: String },
}

#[derive(Deserialize)]
struct Version {
    v: u32,
}

/// Lit le fichier d'un agent du pid `pid` ; `None` s'il a disparu entre-temps.
pub fn read(path: &Path, pid: i32) -> Option<Parsed> {
    let bytes = std::fs::read(path).ok()?;
    Some(parse(&bytes, path, pid))
}

fn parse(bytes: &[u8], path: &Path, pid: i32) -> Parsed {
    let unreadable = || Parsed::Error {
        pid,
        message: format!("{} illisible", path.file_name().unwrap_or(path.as_os_str()).to_string_lossy()),
    };
    let Ok(Version { v }) = serde_json::from_slice(bytes) else { return unreadable() };
    if v != FORMAT_VERSION {
        return Parsed::Error { pid, message: format!("format v{v} non supporté") };
    }
    match serde_json::from_slice(bytes) {
        Ok(rec) => Parsed::Ok(rec),
        Err(_) => unreadable(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{"v":1,"pid":8215,"agent":{"kind":"main","id":"Main","name":"main","depth":0,"parentId":null},"sessionId":"s","cwd":"/tmp/x","title":"","state":"working","zellij":{"session":"m1","paneId":"8"},"startedAt":"2026-10-30T14:50:48.905Z","updatedAt":"2026-10-30T15:58:02.114Z"}"#;

    fn error_of(json: &str) -> String {
        match parse(json.as_bytes(), Path::new("/run/8215/Main.json"), 8215) {
            Parsed::Error { message, .. } => message,
            Parsed::Ok(_) => panic!("parsed"),
        }
    }

    #[test]
    fn unknown_version_is_reported() {
        assert_eq!(error_of(r#"{"v":2,"whatever":true}"#), "format v2 non supporté");
    }

    #[test]
    fn truncated_json_is_unreadable() {
        assert_eq!(error_of(&VALID[..40]), "Main.json illisible");
    }

    #[test]
    fn valid_root_record() {
        let Parsed::Ok(rec) = parse(VALID.as_bytes(), Path::new("Main.json"), 8215) else { panic!("error") };
        assert_eq!(rec.agent.kind, Kind::Main);
        assert_eq!(rec.agent.parent_id, None);
        assert_eq!(rec.state, State::Working);
        assert_eq!(rec.zellij, Some(Zellij { session: "m1".into(), pane_id: "8".into() }));
    }
}
