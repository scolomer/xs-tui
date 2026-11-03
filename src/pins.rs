//! Sessions épinglées : `~/.omp/agent/session-pins.json` et en-têtes des fichiers de session
//! (SPEC.md § Sessions épinglées). agz les épingle avec `p` en suivant le protocole d'écriture
//! d'omp (`session-pins.ts`) : verrou exclusif sur `session-pins.json.lock`, puis fichier
//! temporaire renommé.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

/// Tentatives de prise du verrou des épingles, comme omp (`withFileLock`) mais plus courtes : agz
/// attend sur le thread de l'interface, et omp ne tient le verrou que le temps d'une écriture.
const LOCK_RETRIES: u32 = 20;
const LOCK_RETRY_DELAY: Duration = Duration::from_millis(50);

/// Épingle `id` s'il ne l'est pas, le désépingle sinon ; renvoie le nouvel état. Un nouvel id va en
/// fin de liste, comme avec `/pin`. Un fichier illisible n'est jamais réécrit : omp le traiterait
/// comme vide, et on perdrait toutes les épingles.
pub fn toggle(agent_dir: &Path, id: &str) -> Result<bool, String> {
    let path = agent_dir.join(PINS_FILE);
    let _lock = lock(&agent_dir.join(format!("{PINS_FILE}.lock")))?;
    let mut ids: Vec<String> = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|_| format!("{PINS_FILE} illisible : non modifié"))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(err) => return Err(format!("{PINS_FILE} : {err}")),
    };
    let pinned = !ids.iter().any(|p| p == id);
    if pinned {
        ids.push(id.to_string());
    } else {
        ids.retain(|p| p != id);
    }
    // Même mise en forme qu'omp : `JSON.stringify(ids, null, "\t")`.
    let mut json = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut json, serde_json::ser::PrettyFormatter::with_indent(b"\t"));
    serde::Serialize::serialize(&ids, &mut ser).map_err(|err| err.to_string())?;
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let tmp = agent_dir.join(format!("{PINS_FILE}.{}.{nanos}.tmp", std::process::id()));
    let written = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(&json))
        .and_then(|()| std::fs::rename(&tmp, &path));
    if let Err(err) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{PINS_FILE} : {err}"));
    }
    Ok(pinned)
}

/// Verrou exclusif `flock` sur `path`, relâché à la fermeture du fichier renvoyé.
fn lock(path: &Path) -> Result<File, String> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
        .map_err(|err| format!("{} : {err}", path.display()))?;
    for attempt in 0..LOCK_RETRIES {
        // SAFETY: descripteur ouvert, possédé par `file`.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(file);
        }
        if attempt + 1 < LOCK_RETRIES {
            std::thread::sleep(LOCK_RETRY_DELAY);
        }
    }
    Err("Épingles verrouillées par un autre omp : réessayer".to_string())
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
            let dir = std::env::temp_dir().join(format!("agz-pins-{}-{name}", std::process::id()));
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

    #[test]
    fn toggle_appends_then_removes_keeping_other_pins() {
        let tmp = TempDir::new("toggle");
        assert_eq!(toggle(&tmp.0, "a"), Ok(true));
        write(&tmp.0.join(PINS_FILE), r#"["x","a","y"]"#);
        assert_eq!(toggle(&tmp.0, "a"), Ok(false));
        assert_eq!(load(&tmp.0), ["x", "y"]);
        assert_eq!(toggle(&tmp.0, "z"), Ok(true));
        assert_eq!(std::fs::read_to_string(tmp.0.join(PINS_FILE)).unwrap(), "[\n\t\"x\",\n\t\"y\",\n\t\"z\"\n]");
    }

    #[test]
    fn toggle_leaves_an_unreadable_pins_file_untouched() {
        let tmp = TempDir::new("corrupt");
        write(&tmp.0.join(PINS_FILE), "[\"x\",");
        assert!(toggle(&tmp.0, "a").is_err());
        assert_eq!(std::fs::read_to_string(tmp.0.join(PINS_FILE)).unwrap(), "[\"x\",");
    }
}
