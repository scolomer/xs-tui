//! Fichiers locaux d'xs-parley (SPEC xs-parley §7.5, §7.7) : `~/.omp/xs-parley/` (`XSP_HOME`),
//! identité du session-manager (`manager.json` et `keys/<fp>.key`), parleys jointes par les
//! sessions de la machine (`parleys/<parley_id>.json`), journal d'audit (`audit/<parley_id>.jsonl`).

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use super::codec::{b64u_array, b64u_encode, format_timestamp, is_fp, is_parley_id, new_msg_id, now_ms, random_bytes};
use super::keys::{answer_key, fingerprint, parse, sign_pub, ParleyKey};

/// `XSP_HOME`, sinon `~/.omp/xs-parley`.
pub fn root(home: &Path) -> PathBuf {
    std::env::var_os("XSP_HOME").filter(|v| !v.is_empty()).map_or_else(|| home.join(".omp/xs-parley"), PathBuf::from)
}

pub struct Identity {
    pub fp: String,
    pub seed: [u8; 32],
    /// UUID déclaré comme `session` dans les certificats du manager, tiré une fois.
    pub session: String,
}

/// Crée `dir` et ses parents manquants en 0700.
fn mkdirp(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        mkdirp(parent)?;
    }
    std::fs::create_dir(dir).or_else(|e| if dir.is_dir() { Ok(()) } else { Err(e) })?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Écriture atomique en 0600 : fichier temporaire du même dossier puis `rename`.
fn write_atomic(file: &Path, content: &str) -> std::io::Result<()> {
    mkdirp(file.parent().expect("fichier dans un dossier"))?;
    let tmp = file.with_extension(format!("{}.{}.tmp", std::process::id(), b64u_encode(&random_bytes::<6>())));
    let result = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(content.as_bytes()))
        .and_then(|()| std::fs::rename(&tmp, file));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Lit `manager.json` et `keys/<fp>.key`, ou les crée au premier lancement.
pub fn load_or_create_identity(root: &Path) -> Result<Identity, String> {
    let manager = root.join("manager.json");
    let unreadable = |path: &Path| format!("identité du manager illisible : {}", path.display());
    match std::fs::read_to_string(&manager) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text).map_err(|_| unreadable(&manager))?;
            let (Some(1), Some(fp), Some(session)) = (value["v"].as_u64(), value["fp"].as_str(), value["session"].as_str()) else {
                return Err(unreadable(&manager));
            };
            if !is_fp(fp) || !super::codec::is_session_id(session) {
                return Err(unreadable(&manager));
            }
            let key_file = root.join("keys").join(format!("{fp}.key"));
            let key: Value = std::fs::read_to_string(&key_file)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .ok_or_else(|| unreadable(&key_file))?;
            let seed = key["seed"].as_str().and_then(|s| b64u_array::<32>(s).ok()).ok_or_else(|| unreadable(&key_file))?;
            let public = key["pub"].as_str().and_then(|s| b64u_array::<32>(s).ok()).ok_or_else(|| unreadable(&key_file))?;
            if key["v"].as_u64() != Some(1) || sign_pub(&seed) != public || fingerprint(&public) != fp {
                return Err(unreadable(&key_file));
            }
            Ok(Identity { fp: fp.into(), seed, session: session.into() })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let seed = random_bytes::<32>();
            let public = sign_pub(&seed);
            let fp = fingerprint(&public);
            let session = new_msg_id();
            let key = json!({ "v": 1, "seed": b64u_encode(&seed), "pub": b64u_encode(&public) });
            let write = |path: &Path, value: &Value| {
                write_atomic(path, &format!("{value}\n")).map_err(|e| format!("{} : {e}", path.display()))
            };
            write(&root.join("keys").join(format!("{fp}.key")), &key)?;
            write(&manager, &json!({ "v": 1, "fp": fp, "session": session }))?;
            Ok(Identity { fp, seed, session })
        }
        Err(e) => Err(format!("{} : {e}", manager.display())),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParleyEntry {
    pub parley_id: String,
    pub name: String,
    pub relay: String,
    pub notls: bool,
    /// Clé answer de la parley, dérivée de la clé ask si le fichier n'en a pas d'autre.
    pub answer_key: ParleyKey,
}

/// Parleys de `parleys/` ; un fichier illisible ou sans clé utilisable est compté dans le second
/// élément, sans cacher les autres.
pub fn load_parleys(root: &Path) -> (Vec<ParleyEntry>, usize) {
    let mut out = Vec::new();
    let mut unreadable = 0;
    let Ok(entries) = std::fs::read_dir(root.join("parleys")) else { return (out, 0) };
    let mut names: Vec<String> = entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect();
    names.sort();
    for name in names {
        let Some(id) = name.strip_suffix(".json").filter(|id| is_parley_id(id)) else { continue };
        match read_parley(&root.join("parleys").join(&name), id) {
            Some(entry) => out.push(entry),
            None => unreadable += 1,
        }
    }
    (out, unreadable)
}

fn read_parley(path: &Path, id: &str) -> Option<ParleyEntry> {
    let value: Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    if value["v"].as_u64() != Some(1) || value["parley_id"].as_str() != Some(id) {
        return None;
    }
    let members = value["members"].as_object()?;
    let key = members.values().find_map(|m| parse(m["key"].as_str()?).ok().filter(|k| k.parley_id == id))?;
    Some(ParleyEntry {
        parley_id: id.into(),
        name: value["name"].as_str().unwrap_or(&key.name).into(),
        relay: value["relay"].as_str().unwrap_or(&key.relay).into(),
        notls: value["notls"].as_bool() == Some(true),
        answer_key: answer_key(&key),
    })
}

/// Ajoute une ligne `{ts, parley_id, ...record, session_id}` à `audit/<parley_id>.jsonl` (0600).
/// N'échoue jamais : un disque plein ne doit pas casser le protocole.
pub fn append_audit(root: &Path, parley_id: &str, session_id: &str, record: Map<String, Value>) {
    if !is_parley_id(parley_id) {
        return;
    }
    let mut line = Map::new();
    line.insert("ts".into(), format_timestamp(now_ms()).into());
    line.insert("parley_id".into(), parley_id.into());
    line.extend(record);
    line.insert("session_id".into(), session_id.into());
    let dir = root.join("audit");
    let _ = mkdirp(&dir).and_then(|()| {
        let mut file = OpenOptions::new().append(true).create(true).mode(0o600).open(dir.join(format!("{parley_id}.jsonl")))?;
        file.write_all(format!("{}\n", Value::Object(line)).as_bytes())
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(name: &str) -> TempDir {
            let dir = std::env::temp_dir().join(format!("xst-xsp-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            TempDir(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_identity_is_created_once_then_reloaded_and_checked() {
        let tmp = TempDir::new("identity");
        let first = load_or_create_identity(&tmp.0).unwrap();
        let again = load_or_create_identity(&tmp.0).unwrap();
        assert_eq!((first.fp.as_str(), first.seed, first.session.as_str()), (again.fp.as_str(), again.seed, again.session.as_str()));
        let mode = std::fs::metadata(tmp.0.join("keys").join(format!("{}.key", first.fp))).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        // Une clé qui ne correspond plus à l'empreinte rend l'identité illisible, sans l'écraser.
        let key = tmp.0.join("keys").join(format!("{}.key", first.fp));
        std::fs::write(&key, format!("{}\n", json!({"v": 1, "seed": b64u_encode(&[1u8; 32]), "pub": b64u_encode(&sign_pub(&[1u8; 32]))}))).unwrap();
        assert!(load_or_create_identity(&tmp.0).err().unwrap().contains("illisible"));
    }

    #[test]
    fn parleys_load_with_their_answer_key_and_count_unreadable_files() {
        let tmp = TempDir::new("parleys");
        let g: Value = serde_json::from_str(include_str!("vectors/k-grp.json")).unwrap();
        let id = g["parley_id"].as_str().unwrap();
        let file = json!({
            "v": 1, "parley_id": id, "name": "prod-k8s", "relay": "relay.example.com:8443", "notls": true,
            "members": { "xs:x": { "key": g["key_ask"], "alias": "a", "role": "", "policy": "ask" } }, "peers": {}
        });
        write_atomic(&tmp.0.join("parleys").join(format!("{id}.json")), &file.to_string()).unwrap();
        write_atomic(&tmp.0.join("parleys").join("AAAAAAAAAAAAAAAAAAAAAA.json"), "{").unwrap();
        let (parleys, unreadable) = load_parleys(&tmp.0);
        assert_eq!(unreadable, 1);
        assert_eq!(parleys.len(), 1);
        assert!(parleys[0].notls);
        assert_eq!(crate::xsp::keys::format(&parleys[0].answer_key), g["key_answer"]);
    }
}
