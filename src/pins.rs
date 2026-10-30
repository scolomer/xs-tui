//! Sessions épinglées par `/pin` d'omp : `~/.omp/agent/session-pins.json` et en-têtes des fichiers
//! de session (SPEC.md § Sessions épinglées). agent-tui ne les écrit jamais.

use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Nom du fichier des épingles dans le dossier agent d'omp.
pub const PINS_FILE: &str = "session-pins.json";

/// Octets lus au plus pour trouver l'en-tête d'un fichier de session.
const HEADER_LIMIT: u64 = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pinned {
    pub id: String,
    pub cwd: PathBuf,
    pub title: String,
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    title: Option<String>,
    cwd: Option<PathBuf>,
}

/// Ids épinglés dans l'ordre de `session-pins.json`, sans doublon ; sans le fichier → vide.
/// Une session épinglée avant sa première écriture n'a pas encore de fichier de session.
pub fn load(agent_dir: &Path) -> Vec<String> {
    let Ok(bytes) = std::fs::read(agent_dir.join(PINS_FILE)) else { return Vec::new() };
    let Ok(ids) = serde_json::from_slice::<Vec<String>>(&bytes) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

/// Dossier et titre de la session `id`, lus dans son fichier ; `None` s'il est introuvable ou
/// illisible.
pub fn header(agent_dir: &Path, id: &str) -> Option<Pinned> {
    let (cwd, title) = read_header(&session_file(agent_dir, id)?)?;
    Some(Pinned { id: id.to_string(), cwd, title })
}

/// Fichier `sessions/<dossier>/<ts>_<id>.jsonl` d'une session main ; les sous-agents sont plus bas.
fn session_file(agent_dir: &Path, id: &str) -> Option<PathBuf> {
    let suffix = format!("_{id}.jsonl");
    let dirs = std::fs::read_dir(agent_dir.join("sessions")).ok()?;
    dirs.flatten().find_map(|dir| {
        std::fs::read_dir(dir.path())
            .ok()?
            .flatten()
            .find(|f| f.file_name().to_str().is_some_and(|n| n.ends_with(&suffix)))
            .map(|f| f.path())
    })
}

/// `(cwd, titre)` lus dans les deux premières lignes : `{"type":"title"}` puis `{"type":"session"}`.
/// Sans ligne `session` → `None` ; sans titre → `""`.
fn read_header(path: &Path) -> Option<(PathBuf, String)> {
    let reader = BufReader::new(File::open(path).ok()?.take(HEADER_LIMIT));
    let mut title = String::new();
    for line in reader.lines().take(2) {
        let Ok(line) = serde_json::from_str::<Line>(&line.ok()?) else { continue };
        match line.kind.as_str() {
            "title" => title = line.title.unwrap_or_default(),
            "session" => return line.cwd.map(|cwd| (cwd, title)),
            _ => {}
        }
    }
    None
}

/// Vrai si le bail `run/session-owners/<id>.lock` est tenu par un omp : `flock` exclusif non
/// bloquant, relâché aussitôt s'il est obtenu.
pub fn lease_held(omp_dir: &Path, id: &str) -> bool {
    let Ok(file) = File::open(omp_dir.join("run/session-owners").join(format!("{id}.lock"))) else { return false };
    let fd = file.as_raw_fd();
    // SAFETY: `fd` est un descripteur ouvert, possédé par `file` pendant les deux appels.
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        unsafe { libc::flock(fd, libc::LOCK_UN) };
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EWOULDBLOCK)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dossier temporaire propre au test, supprimé à la fin.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("agent-tui-pins-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn pins_keep_order_without_duplicates_even_without_session_file() {
        let tmp = TempDir::new("ids");
        write(&tmp.0.join(PINS_FILE), r#"["gone","a","gone"]"#);
        assert_eq!(load(&tmp.0), ["gone", "a"]);
        assert_eq!(header(&tmp.0, "gone"), None);
    }

    #[test]
    fn header_reads_title_and_cwd() {
        let tmp = TempDir::new("header");
        write(
            &tmp.0.join("sessions/-tmp-x/2026-10-04T13-18-48-465Z_a.jsonl"),
            "{\"type\":\"title\",\"v\":1,\"title\":\"Hello\"}\n{\"type\":\"session\",\"version\":3,\"id\":\"a\",\"cwd\":\"/tmp/x\"}\n{\"type\":\"message\"}\n",
        );
        assert_eq!(header(&tmp.0, "a"), Some(Pinned { id: "a".into(), cwd: "/tmp/x".into(), title: "Hello".into() }));
    }

    #[test]
    fn missing_title_reads_as_empty() {
        let tmp = TempDir::new("title");
        write(&tmp.0.join("sessions/d/ts_b.jsonl"), "not json\n{\"type\":\"session\",\"id\":\"b\",\"cwd\":\"/tmp/y\"}\n");
        assert_eq!(header(&tmp.0, "b"), Some(Pinned { id: "b".into(), cwd: "/tmp/y".into(), title: String::new() }));
    }

    #[test]
    fn lease_is_held_while_another_descriptor_locks_it() {
        let tmp = TempDir::new("lease");
        let lock = tmp.0.join("run/session-owners/c.lock");
        write(&lock, "");
        assert!(!lease_held(&tmp.0, "missing"));
        let owner = File::open(&lock).unwrap();
        // SAFETY: descripteur ouvert possédé par `owner`.
        assert_eq!(unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
        assert!(lease_held(&tmp.0, "c"));
        drop(owner);
        assert!(!lease_held(&tmp.0, "c"));
    }
}
