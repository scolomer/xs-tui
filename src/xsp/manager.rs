//! Session-manager (SPEC xs-parley §7.7) : thread `xsp-manager`. Rejoint au rang `M` chaque parley
//! de `parleys/`, ouvre les enveloppes reçues et transmet chaque `spawn` valide à xst, puis répond
//! au master par une `response`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use super::cert::{seal_cert_box, sign_manager_cert, Cert};
use super::codec::{format_timestamp, is_session_id, new_msg_id, now_ms};
use super::envelope::{kem_pair, open_envelope, seal_envelope, Header, Plain};
use super::keys::{endpoint, group_key, sign_pub};
use super::relay::{ConnEvent, Connection};
use super::store::{append_audit, load_parleys, Identity, ParleyEntry};
use super::{Command, ManagerStatus, SpawnRequest, XspEvent};

/// Rotation des paires X25519, comme les sessions.
const KEM_ROTATE: Duration = Duration::from_secs(24 * 3600);
/// Durée de vie d'une paire remplacée, et du souvenir d'un `msg_id` (anti-rejeu).
const GRACE: Duration = Duration::from_secs(10 * 60);
/// Au-delà, un `spawn` sans `Command::Spawned` reçoit une erreur.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(90);
const TICK: Duration = Duration::from_secs(1);
const ROLE_MAX: usize = 280;

pub enum Input {
    Cmd(Command),
    Conn(ConnEvent),
}

impl From<ConnEvent> for Input {
    fn from(event: ConnEvent) -> Input {
        Input::Conn(event)
    }
}

struct KemKey {
    private: [u8; 32],
    public: [u8; 32],
    /// Remplacée à cet instant ; `None` pour la paire courante.
    retired: Option<Instant>,
}

/// Une connexion au relai et ses paires X25519 (jamais écrites sur disque).
struct Link {
    conn: Connection,
    up: bool,
    /// Paire courante en premier.
    kem: Vec<KemKey>,
    rotate_at: Instant,
}

impl Link {
    fn rotate(&mut self, now: Instant) {
        for key in &mut self.kem {
            key.retired.get_or_insert(now);
        }
        let (private, public) = kem_pair();
        self.kem.insert(0, KemKey { private, public, retired: None });
        self.rotate_at = now + KEM_ROTATE;
    }
}

struct Joined {
    entry: ParleyEntry,
    endpoint: String,
    /// Certificat publié au relai, signé à la dernière connexion ou rotation.
    cert: Option<Cert>,
    /// Une autre connexion a repris l'empreinte : plus rejointe jusqu'au prochain `Reload`.
    replaced: bool,
}

struct PendingSpawn {
    parley_id: String,
    msg_id: String,
    from_fp: String,
    from_cert: Cert,
    at: Instant,
}

pub struct State {
    root: PathBuf,
    identity: Identity,
    sign_pub: [u8; 32],
    alias: String,
    role: String,
    parleys: HashMap<String, Joined>,
    links: HashMap<String, Link>,
    seen: HashMap<String, Instant>,
    pending: HashMap<u64, PendingSpawn>,
    next_id: u64,
    unreadable: usize,
    /// Dernier problème par endpoint (relai injoignable, trame d'erreur).
    problems: HashMap<String, String>,
    replaced: bool,
    last_status: Option<ManagerStatus>,
    emit: Box<dyn FnMut(XspEvent) + Send>,
    connect: Box<dyn FnMut(&str) -> Connection + Send>,
}

/// Premier label du nom d'hôte, tel quel ; `None` si `gethostname` échoue.
fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY : `buf` est valide pour `buf.len()` octets ; le résultat est terminé par un octet nul
    // ou tronqué, et lu jusqu'au premier nul au plus.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).into_owned();
    Some(name.split('.').next().unwrap_or_default().to_string()).filter(|n| !n.is_empty())
}

/// `(alias, role)` : `xst-<hôte>` conforme aux alias (§1), 32 caractères au plus, `xst` sans hôte ;
/// `session-manager xst (<hôte>)`.
pub fn alias_and_role(host: Option<&str>) -> (String, String) {
    let Some(host) = host else { return ("xst".into(), "session-manager xst".into()) };
    let slug: String =
        host.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '-' }).collect();
    let alias: String = format!("xst-{slug}").chars().take(32).collect();
    let clean: String = host.chars().filter(|c| !c.is_control() && *c != '\u{2028}' && *c != '\u{2029}').collect();
    let role: String = format!("session-manager xst ({clean})").chars().take(ROLE_MAX).collect();
    (alias, role)
}

fn endpoint_host(endpoint: &str) -> &str {
    endpoint.split("://").nth(1).and_then(|r| r.strip_suffix("/ws")).unwrap_or(endpoint)
}

fn record(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs.iter().filter(|(_, v)| !v.is_null()).map(|(k, v)| ((*k).to_string(), v.clone())).collect()
}

impl State {
    pub fn new(
        root: PathBuf,
        identity: Identity,
        emit: Box<dyn FnMut(XspEvent) + Send>,
        connect: Box<dyn FnMut(&str) -> Connection + Send>,
    ) -> State {
        let (alias, role) = alias_and_role(hostname().as_deref());
        State {
            root,
            sign_pub: sign_pub(&identity.seed),
            identity,
            alias,
            role,
            parleys: HashMap::new(),
            links: HashMap::new(),
            seen: HashMap::new(),
            pending: HashMap::new(),
            next_id: 0,
            unreadable: 0,
            problems: HashMap::new(),
            replaced: false,
            last_status: None,
            emit,
            connect,
        }
    }

    /// Boucle du thread : entrées jusqu'à la fin du canal, tic chaque seconde.
    pub fn run(mut self, inputs: &Receiver<Input>) {
        loop {
            match inputs.recv_timeout(TICK) {
                Ok(input) => self.handle(input),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            self.tick(Instant::now());
        }
    }

    pub fn handle(&mut self, input: Input) {
        match input {
            Input::Cmd(Command::Reload) => self.reload(),
            Input::Cmd(Command::Spawned { id, result }) => self.spawned(id, result),
            Input::Conn(ConnEvent::Up(endpoint)) => self.up(&endpoint),
            Input::Conn(ConnEvent::Down(endpoint, reason)) => {
                if let Some(link) = self.links.get_mut(&endpoint) {
                    link.up = false;
                }
                let problem = format!("relai {} : {reason}", endpoint_host(&endpoint));
                self.problems.insert(endpoint, problem);
            }
            Input::Conn(ConnEvent::Problem(endpoint, message)) => {
                let problem = format!("relai {} : {message}", endpoint_host(&endpoint));
                self.problems.insert(endpoint, problem);
            }
            Input::Conn(ConnEvent::Replaced(endpoint, parley_id)) => {
                if let Some(joined) = self.parleys.get_mut(&parley_id).filter(|j| j.endpoint == endpoint) {
                    joined.replaced = true;
                    self.replaced = true;
                }
            }
            Input::Conn(ConnEvent::Deliver(endpoint, parley_id, env)) => self.deliver(&endpoint, &parley_id, &env),
        }
        self.status();
    }

    fn status(&mut self) {
        let parleys = self.parleys.values().filter(|j| !j.replaced).count();
        let mut problems: Vec<&String> = self.problems.values().collect();
        problems.sort();
        let problem = if self.replaced {
            Some("identité du manager reprise par une autre connexion (autre xst avec le même manager.json ?)".to_string())
        } else if let Some(p) = problems.first() {
            Some((*p).clone())
        } else if self.unreadable > 0 {
            Some(format!("{} fichier(s) de parley illisible(s)", self.unreadable))
        } else {
            None
        };
        let status = ManagerStatus { parleys, problem };
        if self.last_status.as_ref() != Some(&status) {
            self.last_status = Some(status.clone());
            (self.emit)(XspEvent::Status(status));
        }
    }

    fn audit(&self, parley_id: &str, pairs: &[(&str, Value)]) {
        append_audit(&self.root, parley_id, &self.identity.session, record(pairs));
    }

    fn reload(&mut self) {
        let (entries, unreadable) = load_parleys(&self.root);
        self.unreadable = unreadable;
        self.replaced = false;
        let wanted: HashMap<String, ParleyEntry> = entries.into_iter().map(|e| (e.parley_id.clone(), e)).collect();
        // Parleys disparues ou modifiées : quittées.
        let gone: Vec<String> = self
            .parleys
            .iter()
            .filter(|(id, j)| wanted.get(*id) != Some(&j.entry))
            .map(|(id, _)| id.clone())
            .collect();
        for id in gone {
            let joined = self.parleys.remove(&id).expect("parley connue");
            if let Some(link) = self.links.get(&joined.endpoint).filter(|l| l.up && !joined.replaced) {
                link.conn.send_frame(json!({ "t": "leave", "parley_id": id }));
            }
        }
        for joined in self.parleys.values_mut() {
            joined.replaced = false;
        }
        // Parleys nouvelles : rejointes si leur relai est connecté, sinon à sa connexion.
        let mut fresh = Vec::new();
        for (id, entry) in wanted {
            if self.parleys.contains_key(&id) {
                continue;
            }
            let endpoint = endpoint(&entry.relay, entry.notls);
            self.parleys.insert(id.clone(), Joined { entry, endpoint, cert: None, replaced: false });
            fresh.push(id);
        }
        // Une connexion par endpoint utilisé ; les autres sont libérées (leur thread s'arrête).
        let used: std::collections::HashSet<String> = self.parleys.values().map(|j| j.endpoint.clone()).collect();
        self.links.retain(|e, _| used.contains(e));
        self.problems.retain(|e, _| used.contains(e));
        for endpoint in used {
            if !self.links.contains_key(&endpoint) {
                let conn = (self.connect)(&endpoint);
                let now = Instant::now();
                let mut link = Link { conn, up: false, kem: Vec::new(), rotate_at: now + KEM_ROTATE };
                link.rotate(now);
                self.links.insert(endpoint, link);
            }
        }
        for id in fresh {
            if self.links.get(&self.parleys[&id].endpoint).is_some_and(|l| l.up) {
                self.join(&id);
            }
        }
    }

    /// Signe un certificat `M` avec la paire X25519 courante et rejoint la parley `id`.
    fn join(&mut self, id: &str) {
        let Some(joined) = self.parleys.get_mut(id).filter(|j| !j.replaced) else { return };
        let Some(link) = self.links.get(&joined.endpoint) else { return };
        let key = &joined.entry.answer_key;
        let kem_pub = link.kem[0].public;
        let cert = sign_manager_cert(key, &self.sign_pub, &kem_pub, &self.alias, &self.role, &self.identity.session, now_ms());
        let cert_box = seal_cert_box(&group_key(key), id, &cert);
        link.conn.send_frame(json!({ "t": "join", "parley_id": id, "fp": self.identity.fp, "cert_box": cert_box }));
        joined.cert = Some(cert);
    }

    fn join_all(&mut self, endpoint: &str) {
        let ids: Vec<String> = self.parleys.iter().filter(|(_, j)| j.endpoint == endpoint).map(|(id, _)| id.clone()).collect();
        for id in ids {
            self.join(&id);
        }
    }

    fn up(&mut self, endpoint: &str) {
        let Some(link) = self.links.get_mut(endpoint) else { return };
        link.up = true;
        // Nouvelle paire à chaque connexion ; la précédente déchiffre encore 10 min.
        link.rotate(Instant::now());
        self.problems.remove(endpoint);
        self.join_all(endpoint);
    }

    fn deliver(&mut self, endpoint: &str, parley_id: &str, env: &Value) {
        let Some(joined) = self.parleys.get(parley_id).filter(|j| j.endpoint == endpoint && !j.replaced) else { return };
        let (key, parley_name) = (joined.entry.answer_key.clone(), joined.entry.name.clone());
        let Some(link) = self.links.get(endpoint) else { return };
        let now = Instant::now();
        let privs: Vec<[u8; 32]> =
            link.kem.iter().filter(|k| k.retired.is_none_or(|at| now - at < GRACE)).map(|k| k.private).collect();
        let peer_fp = env["h"]["from"].as_str().map(str::to_string);
        let opened = open_envelope(env, parley_id, &self.identity.fp, &key, &privs, now_ms());
        let (h, plain, cert) = match opened {
            Ok(opened) => opened,
            Err(code) => {
                self.audit(parley_id, &[("event", "rejected".into()), ("reason", code.into()), ("peer_fp", peer_fp.into())]);
                return;
            }
        };
        let peer = [("peer_alias", Value::from(cert.body.alias.as_str())), ("peer_fp", h.from.as_str().into())];
        if self.seen.contains_key(&h.msg_id) {
            self.audit(parley_id, &[("event", "rejected".into()), ("reason", "replay".into()), ("msg_id", h.msg_id.as_str().into()), peer[0].clone(), peer[1].clone()]);
            return;
        }
        self.seen.insert(h.msg_id.clone(), now);
        let rejected = |reason: String| [("event", Value::from("rejected")), ("reason", reason.into()), ("msg_id", h.msg_id.as_str().into()), peer[0].clone(), peer[1].clone()];
        match h.kind.as_str() {
            "spawn" if cert.body.rank != "A" => self.audit(parley_id, &rejected("spawn_from_non_master".into())),
            "spawn" if !is_session_id(&plain.text) => {
                let pending = PendingSpawn { parley_id: parley_id.into(), msg_id: h.msg_id.clone(), from_fp: h.from.clone(), from_cert: cert, at: now };
                self.reply(&pending, "error", Some("session invalide"));
            }
            "spawn" => {
                self.audit(parley_id, &[("event", "spawn_received".into()), ("msg_id", h.msg_id.as_str().into()), peer[0].clone(), peer[1].clone(), ("text", plain.text.as_str().into())]);
                let id = self.next_id;
                self.next_id += 1;
                let request = SpawnRequest {
                    id,
                    session: plain.text.clone(),
                    from_alias: cert.body.alias.clone(),
                    parley_name,
                };
                self.pending.insert(id, PendingSpawn { parley_id: parley_id.into(), msg_id: h.msg_id.clone(), from_fp: h.from.clone(), from_cert: cert, at: now });
                (self.emit)(XspEvent::Spawn(request));
            }
            "request" => {
                self.audit(parley_id, &rejected("request_to_manager".into()));
                let pending = PendingSpawn { parley_id: parley_id.into(), msg_id: h.msg_id.clone(), from_fp: h.from.clone(), from_cert: cert, at: now };
                let _ = self.reply_raw(&pending, "error", Some("session-manager : seules les enveloppes spawn sont acceptées"));
            }
            kind => self.audit(parley_id, &rejected(format!("{kind}_to_manager"))),
        }
    }

    fn spawned(&mut self, id: u64, result: Result<Option<String>, String>) {
        let Some(pending) = self.pending.remove(&id) else { return };
        match &result {
            Ok(note) => self.reply(&pending, "ok", note.as_deref()),
            Err(reason) => self.reply(&pending, "error", Some(reason)),
        }
    }

    /// Répond au `spawn` de `p` et l'inscrit au journal (`spawned` ou `spawn_failed`).
    fn reply(&mut self, p: &PendingSpawn, status: &str, reason: Option<&str>) {
        let msg_id = self.reply_raw(p, status, reason);
        let event = if status == "ok" { "spawned" } else { "spawn_failed" };
        self.audit(
            &p.parley_id,
            &[
                ("event", event.into()),
                ("msg_id", msg_id.into()),
                ("reply_to", p.msg_id.as_str().into()),
                ("peer_alias", p.from_cert.body.alias.as_str().into()),
                ("peer_fp", p.from_fp.as_str().into()),
                ("status", status.into()),
                ("reason", reason.into()),
            ],
        );
    }

    /// Envoie la `response` à `p.from_fp`, chiffrée pour la clé X25519 de son certificat ; renvoie son
    /// `msg_id`, `None` si elle n'a pas pu partir (parley quittée, relai coupé).
    fn reply_raw(&mut self, p: &PendingSpawn, status: &str, reason: Option<&str>) -> Option<String> {
        let joined = self.parleys.get(&p.parley_id)?;
        let cert = joined.cert.clone()?;
        let link = self.links.get(&joined.endpoint).filter(|l| l.up)?;
        let h = Header {
            v: 1,
            parley_id: p.parley_id.clone(),
            msg_id: new_msg_id(),
            kind: "response".into(),
            from: self.identity.fp.clone(),
            to: p.from_fp.clone(),
            reply_to: Some(p.msg_id.clone()),
            ts: format_timestamp(now_ms()),
        };
        let plain = Plain { cert, text: String::new(), status: Some(status.into()), reason: reason.map(Into::into) };
        let env = seal_envelope(&h, &plain, &p.from_cert.kem_pub().ok()?, &self.identity.seed).ok()?;
        link.conn.send_frame(json!({ "t": "send", "parley_id": p.parley_id, "to": p.from_fp, "env": env }));
        Some(h.msg_id)
    }

    /// Purge l'anti-rejeu et les paires remplacées, fait tourner les paires, répond aux `spawn`
    /// restés sans réponse d'xst.
    pub fn tick(&mut self, now: Instant) {
        self.seen.retain(|_, at| now - *at < GRACE);
        let mut rotate = Vec::new();
        for (endpoint, link) in &mut self.links {
            link.kem.retain(|k| k.retired.is_none_or(|at| now - at < GRACE));
            if link.up && now >= link.rotate_at {
                link.rotate(now);
                rotate.push(endpoint.clone());
            }
        }
        for endpoint in rotate {
            self.join_all(&endpoint);
        }
        let late: Vec<u64> = self.pending.iter().filter(|(_, p)| now - p.at >= SPAWN_TIMEOUT).map(|(id, _)| *id).collect();
        for id in late {
            let pending = self.pending.remove(&id).expect("spawn en attente");
            self.reply(&pending, "error", Some("xst n'a pas répondu"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Arc, Mutex};

    use ed25519_dalek::{Signer, SigningKey};

    use super::*;
    use crate::xsp::cert::CertBody;
    use crate::xsp::codec::{b64u_encode, jcs};
    use crate::xsp::keys::{fingerprint, parse, ParleyKey};
    use crate::xsp::store::load_or_create_identity;

    struct Master {
        key: ParleyKey,
        seed: [u8; 32],
        fp: String,
        cert: Cert,
        kem_private: [u8; 32],
    }

    fn master(key: ParleyKey) -> Master {
        let seed = [3u8; 32];
        let public = sign_pub(&seed);
        let (kem_private, kem_public) = kem_pair();
        let body = CertBody {
            v: 1,
            parley_id: key.parley_id.clone(),
            rank: "A".into(),
            sign_pub: b64u_encode(&public),
            kem_pub: b64u_encode(&kem_public),
            alias: "alice".into(),
            role: "master".into(),
            session: "019a5f3c-1b2d-7e4f-8a6b-1c2d3e4f5a6b".into(),
            issued_at: format_timestamp(now_ms()),
        };
        let sig = SigningKey::from_bytes(&key.seed).sign(jcs(&serde_json::to_value(&body).unwrap()).as_bytes());
        Master { fp: fingerprint(&public), cert: Cert { body, sig: b64u_encode(&sig.to_bytes()) }, key, seed, kem_private }
    }

    fn envelope(m: &Master, to: &str, kem_pub: &[u8; 32], kind: &str, text: &str) -> Value {
        let h = Header {
            v: 1,
            parley_id: m.key.parley_id.clone(),
            msg_id: new_msg_id(),
            kind: kind.into(),
            from: m.fp.clone(),
            to: to.into(),
            reply_to: None,
            ts: format_timestamp(now_ms()),
        };
        let plain = Plain { cert: m.cert.clone(), text: text.into(), status: None, reason: None };
        seal_envelope(&h, &plain, kem_pub, &m.seed).unwrap()
    }

    fn frames(rx: &Receiver<String>) -> Vec<Value> {
        rx.try_iter().map(|f| serde_json::from_str(&f).unwrap()).collect()
    }

    #[test]
    fn a_spawn_reaches_xst_and_its_response_opens_for_the_master() {
        let root = std::env::temp_dir().join(format!("xst-xsp-manager-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let g: Value = serde_json::from_str(include_str!("vectors/k-grp.json")).unwrap();
        let ask = parse(g["key_ask"].as_str().unwrap()).unwrap();
        let id = ask.parley_id.clone();
        let file = json!({ "v": 1, "parley_id": id, "name": "prod-k8s", "relay": ask.relay, "members": { "xs:m": { "key": g["key_ask"] } }, "peers": {} });
        std::fs::create_dir_all(root.join("parleys")).unwrap();
        std::fs::write(root.join("parleys").join(format!("{id}.json")), file.to_string()).unwrap();

        let identity = load_or_create_identity(&root).unwrap();
        let manager_fp = identity.fp.clone();
        let (events_tx, events) = mpsc::channel();
        let outs: Arc<Mutex<Vec<Receiver<String>>>> = Arc::default();
        let captured = Arc::clone(&outs);
        let mut state = State::new(
            root.clone(),
            identity,
            Box::new(move |e| events_tx.send(e).unwrap()),
            Box::new(move |_| {
                let (conn, rx) = Connection::detached();
                captured.lock().unwrap().push(rx);
                conn
            }),
        );
        state.handle(Input::Cmd(Command::Reload));
        let ep = endpoint(&ask.relay, false);
        state.handle(Input::Conn(ConnEvent::Up(ep.clone())));
        let out = outs.lock().unwrap().pop().unwrap();
        let joins = frames(&out);
        assert_eq!(joins.len(), 1);
        assert_eq!((joins[0]["t"].as_str(), joins[0]["fp"].as_str()), (Some("join"), Some(manager_fp.as_str())));
        assert!(matches!(events.try_iter().last(), Some(XspEvent::Status(ManagerStatus { parleys: 1, problem: None }))));

        let m = master(ask.clone());
        let kem_pub = state.parleys[&id].cert.as_ref().unwrap().kem_pub().unwrap();
        let session = "019a5f41-0c9e-7d3a-9b5f-2e3d4c5b6a7f";
        let spawn = envelope(&m, &manager_fp, &kem_pub, "spawn", session);
        state.handle(Input::Conn(ConnEvent::Deliver(ep.clone(), id.clone(), spawn.clone())));
        let request = match events.try_iter().find(|e| matches!(e, XspEvent::Spawn(_))) {
            Some(XspEvent::Spawn(r)) => r,
            _ => panic!("pas de spawn"),
        };
        assert_eq!((request.session.as_str(), request.from_alias.as_str(), request.parley_name.as_str()), (session, "alice", "prod-k8s"));
        // Rejeu : ignoré.
        state.handle(Input::Conn(ConnEvent::Deliver(ep.clone(), id.clone(), spawn.clone())));
        assert!(!events.try_iter().any(|e| matches!(e, XspEvent::Spawn(_))));

        state.handle(Input::Cmd(Command::Spawned { id: request.id, result: Ok(Some("déjà ouverte".into())) }));
        let sent = frames(&out);
        assert_eq!(sent.len(), 1);
        assert_eq!((sent[0]["t"].as_str(), sent[0]["to"].as_str()), (Some("send"), Some(m.fp.as_str())));
        let (h, plain, cert) = open_envelope(&sent[0]["env"], &id, &m.fp, &ask, &[m.kem_private], now_ms()).unwrap();
        assert_eq!((h.kind.as_str(), h.reply_to.as_deref()), ("response", spawn["h"]["msg_id"].as_str()));
        assert_eq!((plain.status.as_deref(), plain.reason.as_deref()), (Some("ok"), Some("déjà ouverte")));
        assert_eq!(cert.body.rank, "M");

        // Une request reçoit une erreur, sans passer par xst.
        let request_env = envelope(&m, &manager_fp, &kem_pub, "request", "fais ceci");
        state.handle(Input::Conn(ConnEvent::Deliver(ep, id.clone(), request_env)));
        assert!(!events.try_iter().any(|e| matches!(e, XspEvent::Spawn(_))));
        let sent = frames(&out);
        let (_, plain, _) = open_envelope(&sent[0]["env"], &id, &m.fp, &ask, &[m.kem_private], now_ms()).unwrap();
        assert_eq!(plain.status.as_deref(), Some("error"));
        let audit = std::fs::read_to_string(root.join("audit").join(format!("{id}.jsonl"))).unwrap();
        for event in ["spawn_received", "\"reason\":\"replay\"", "spawned", "request_to_manager"] {
            assert!(audit.contains(event), "{event} absent du journal");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn aliases_follow_the_name_profile() {
        assert_eq!(alias_and_role(Some("Mac de Sylvain")).0, "xst-Mac-de-Sylvain");
        assert_eq!(alias_and_role(Some(&"a".repeat(40))).0.len(), 32);
        assert_eq!(alias_and_role(None), ("xst".into(), "session-manager xst".into()));
    }
}
