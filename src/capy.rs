//! Threads Capy de l'utilisateur (SPEC.md § Threads Capy), lus par l'API publique de Capy avec la
//! clé `CAPY_API_KEY` : affichage seulement. L'API n'a ni liste de compte ni flux de liste : la
//! liste complète est relue toutes les 60 s (projets, puis threads de chaque projet), et les threads
//! connus toutes les 10 s.

use std::collections::HashMap;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::watch::Event;

const API: &str = "https://api.capy.ai/api/v1";
/// Relecture de toutes les listes : découvre les threads nouveaux ou rejoints.
const FULL_SCAN: Duration = Duration::from_secs(60);
/// Relecture des threads déjà affichés : suit leur état.
const REFRESH: Duration = Duration::from_secs(10);
const PAGE: usize = 100;
/// Projets lus en parallèle pendant une relecture complète.
const PARALLEL: usize = 8;
const TIMEOUT: Duration = Duration::from_secs(20);
/// Erreur d'un thread introuvable : supprimé, ou plus accessible.
const NOT_FOUND: &str = "thread introuvable";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Working,
    Waiting,
    Idle,
    Failed,
    Archived,
}

/// Thread affiché : seulement ceux de l'utilisateur, non archivés.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Thread {
    pub id: String,
    /// Nom du projet Capy.
    pub project: String,
    pub title: String,
    pub status: Status,
    /// Capy ou un coéquipier attend une réponse de l'utilisateur.
    pub needs_you: bool,
    pub unread: bool,
    /// Création, au format ISO 8601 comme `startedAt` des instances omp : sert à l'ordre.
    pub created_at: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiThread {
    id: String,
    project_id: Option<String>,
    title: Option<String>,
    status: Status,
    archived: bool,
    #[serde(default)]
    needs_you: bool,
    #[serde(default)]
    has_unreads: bool,
    /// Threads que l'utilisateur a lancés, où il a écrit ou été mentionné.
    #[serde(default)]
    participated: bool,
    #[serde(default)]
    member: bool,
    created_at: String,
}

#[derive(Deserialize)]
struct Page<T> {
    items: Vec<T>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
struct Project {
    id: String,
    name: String,
}

/// Le thread de l'API s'il est affiché : à l'utilisateur et non archivé.
fn keep(t: ApiThread, projects: &HashMap<String, String>) -> Option<Thread> {
    if t.archived || t.status == Status::Archived || !(t.participated || t.member) {
        return None;
    }
    let project = t.project_id.as_ref().and_then(|p| projects.get(p)).cloned().unwrap_or_default();
    Some(Thread {
        id: t.id,
        project,
        title: t.title.unwrap_or_default(),
        status: t.status,
        needs_you: t.needs_you,
        unread: t.has_unreads,
        created_at: t.created_at,
    })
}

/// Accès à l'API Capy avec la clé de l'utilisateur ; partagé par la relecture de la liste et la
/// conversation de la copie `d`.
#[derive(Clone)]
pub struct Api {
    agent: ureq::Agent,
    /// Sans délai global : un flux reste ouvert tant que la conversation est affichée.
    streaming: ureq::Agent,
    auth: String,
}

impl Api {
    /// `None` sans `CAPY_API_KEY` : xst n'affiche aucun thread et n'appelle pas l'API.
    pub fn from_env() -> Option<Api> {
        let key = std::env::var("CAPY_API_KEY").ok().filter(|k| !k.trim().is_empty())?;
        let agent = ureq::Agent::config_builder().timeout_global(Some(TIMEOUT)).build().into();
        let streaming =
            ureq::Agent::config_builder().timeout_connect(Some(TIMEOUT)).timeout_recv_response(Some(TIMEOUT)).build().into();
        Some(Api { agent, streaming, auth: format!("Bearer {}", key.trim()) })
    }

    pub fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let mut response =
            self.agent.get(&format!("{API}{path}")).header("Authorization", &self.auth).call().map_err(describe)?;
        let body = response.body_mut().read_to_string().map_err(|e| format!("réponse illisible : {e}"))?;
        serde_json::from_str(&body).map_err(|e| format!("réponse illisible : {e}"))
    }

    /// Ouvre le flux Server-Sent Events `path`, repris après l'événement `after` ; sans compression,
    /// pour recevoir chaque trame dès qu'elle part.
    pub fn stream(&self, path: &str, after: Option<&str>) -> Result<impl std::io::Read + use<>, String> {
        let mut request = self
            .streaming
            .get(&format!("{API}{path}"))
            .header("Authorization", &self.auth)
            .header("Accept", "text/event-stream")
            .header("Accept-Encoding", "identity");
        if let Some(id) = after {
            request = request.header("Last-Event-ID", id);
        }
        Ok(request.call().map_err(describe)?.into_body().into_reader())
    }

    /// Envoie `text` au thread `id` comme un message tapé dans Capy : *steer*, le mode par défaut de
    /// l'app, qui rejoint l'agent à son étape suivante sans l'interrompre. L'API, elle, interrompt
    /// par défaut : le mode est donc toujours précisé.
    pub fn send_message(&self, id: &str, text: &str) -> Result<(), String> {
        let body = serde_json::json!({ "text": text, "delivery": "steer" }).to_string();
        self.agent
            .post(&format!("{API}/threads/{id}/message"))
            .header("Authorization", &self.auth)
            .header("Content-Type", "application/json")
            .send(body.as_str())
            .map_err(describe)?;
        Ok(())
    }

    fn projects(&self) -> Result<HashMap<String, String>, String> {
        let page: Page<Project> = self.get("/projects")?;
        Ok(page.items.into_iter().map(|p| (p.id, p.name)).collect())
    }

    /// Tous les threads du projet `id`, toutes pages lues.
    fn project_threads(&self, id: &str) -> Result<Vec<ApiThread>, String> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut path = format!("/threads?projectId={id}&limit={PAGE}");
            if let Some(c) = &cursor {
                path.push_str(&format!("&cursor={}", encode(c)));
            }
            let page: Page<ApiThread> = self.get(&path)?;
            out.extend(page.items);
            match page.cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(out),
            }
        }
    }

    /// Projets, puis leurs threads, `PARALLEL` projets à la fois.
    fn full_scan(&self) -> Result<(HashMap<String, String>, Vec<Thread>), String> {
        let projects = self.projects()?;
        let ids: Vec<&String> = projects.keys().collect();
        let mut threads = Vec::new();
        for chunk in ids.chunks(PARALLEL) {
            let results: Vec<Result<Vec<ApiThread>, String>> = std::thread::scope(|s| {
                let handles: Vec<_> = chunk.iter().map(|id| s.spawn(|| self.project_threads(id))).collect();
                handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("lecture interrompue".into()))).collect()
            });
            for result in results {
                threads.extend(result?.into_iter().filter_map(|t| keep(t, &projects)));
            }
        }
        Ok((projects, threads))
    }

    /// Relit les threads affichés ; un thread archivé, quitté ou supprimé disparaît.
    fn refresh(&self, threads: &[Thread], projects: &HashMap<String, String>) -> Result<Vec<Thread>, String> {
        std::thread::scope(|s| {
            let handles: Vec<_> = threads
                .iter()
                .map(|t| {
                    let id = &t.id;
                    s.spawn(move || match self.get::<ApiThread>(&format!("/threads/{id}")) {
                        Ok(t) => Ok(keep(t, projects)),
                        Err(e) if e == NOT_FOUND => Ok(None),
                        Err(e) => Err(e),
                    })
                })
                .collect();
            let mut out = Vec::new();
            for handle in handles {
                out.extend(handle.join().unwrap_or_else(|_| Err("lecture interrompue".into()))?);
            }
            Ok(out)
        })
    }
}

/// Message d'une erreur de l'API, pour la barre du bas ou la copie.
fn describe(e: ureq::Error) -> String {
    match e {
        ureq::Error::StatusCode(401) => "clé CAPY_API_KEY refusée".to_string(),
        ureq::Error::StatusCode(404) => NOT_FOUND.to_string(),
        ureq::Error::StatusCode(429) => "trop de requêtes".to_string(),
        e => e.to_string(),
    }
}

/// Encodage pour un paramètre d'URL : tout sauf `[A-Za-z0-9_.~-]`.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Ouvre le thread `id` dans l'app desktop Capy par son lien profond `capy://app/thread/<id>`, le
/// chemin de la page du thread dans l'app (`/thread/<id>`) derrière le schéma qu'elle déclare.
pub fn open_in_app(id: &str) -> Result<(), String> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(format!("Thread Capy invalide : {id}"));
    }
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let status = std::process::Command::new(opener)
        .arg(format!("capy://app/thread/{id}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("{opener} : {e}"))?;
    if status.success() { Ok(()) } else { Err("App Capy introuvable : lien capy:// non ouvert".to_string()) }
}

/// Lance le thread qui relit les threads de l'utilisateur.
pub fn spawn_refresher(api: Api, tx: Sender<Event>) {
    std::thread::Builder::new()
        .name("capy".into())
        .spawn(move || refresh_loop(&api, &tx))
        .expect("thread Capy");
}

fn refresh_loop(client: &Api, tx: &Sender<Event>) {
    let mut projects = HashMap::new();
    let mut threads: Vec<Thread> = Vec::new();
    let mut scanned: Option<Instant> = None;
    loop {
        let full = scanned.is_none_or(|at| at.elapsed() >= FULL_SCAN);
        let result = if full {
            client.full_scan().map(|(p, t)| {
                projects = p;
                scanned = Some(Instant::now());
                t
            })
        } else {
            client.refresh(&threads, &projects)
        };
        // Après un échec, les derniers threads connus restent affichés.
        if let Ok(fresh) = &result {
            threads = fresh.clone();
        }
        if tx.send(Event::Capy(result)).is_err() {
            return;
        }
        std::thread::sleep(REFRESH);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_users_unarchived_threads_are_kept_with_their_project_name() {
        let projects = HashMap::from([("p1".to_string(), "Selection".to_string())]);
        let thread = |extra: &str| -> ApiThread {
            serde_json::from_str(&format!(
                r#"{{"id":"jam_1","projectId":"p1","title":"T","status":"waiting","archived":false,
                "needsYou":true,"hasUnreads":false,"createdAt":"2026-11-18T09:40:03.531Z","unknownField":1{extra}}}"#
            ))
            .unwrap()
        };
        let kept = keep(thread(r#","participated":true,"member":true"#), &projects).unwrap();
        assert_eq!((kept.project.as_str(), kept.status, kept.needs_you), ("Selection", Status::Waiting, true));
        assert_eq!(keep(thread(r#","participated":false,"member":false"#), &projects), None);
        let mut archived = thread(r#","participated":true"#);
        archived.archived = true;
        assert_eq!(keep(archived, &projects), None);
    }

    #[test]
    fn cursors_are_url_encoded() {
        assert_eq!(encode("1763481234567:jam_01/x+y"), "1763481234567%3Ajam_01%2Fx%2By");
    }
}
