//! Branche git d'un dossier et état propre/modifié, lus avec gix (aucun process git lancé).
//!
//! Le statut parcourt le worktree : il est calculé sur un thread dédié, toutes les 10 s comme la
//! barre d'état d'omp, et aussitôt sur demande (HEAD ou index modifié, nouvelle publication).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use crate::watch::Event;

/// Cadence de rafraîchissement du statut, celle d'omp (`GIT_STATUS_TTL_MS`).
const STATUS_TTL: Duration = Duration::from_secs(10);

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Repo {
    /// Gitdir propre au worktree (`.git/worktrees/<n>` pour un worktree) : son HEAD est surveillé.
    pub git_dir: PathBuf,
    pub label: Option<String>,
    /// Fichiers indexés, modifiés ou non suivis, comme le segment git d'omp.
    pub dirty: bool,
}

/// Dépôt contenant `cwd`, ou `None` hors dépôt.
pub fn inspect(cwd: &Path) -> Option<Repo> {
    let repo = gix::discover(cwd).ok()?;
    Some(Repo { git_dir: repo.git_dir().to_path_buf(), label: head_label(&repo), dirty: is_dirty(&repo) })
}

/// Équivalent de `git branch --show-current`, ou `@<git rev-parse --short HEAD>` si HEAD est détaché.
/// Recopié de `head_label` d'omp-sessions, sans l'abréviation du nom de branche.
fn head_label(repo: &gix::Repository) -> Option<String> {
    match repo.head_name().ok()? {
        Some(name) => Some(name.shorten().to_string()),
        None => Some(format!("@{}", repo.head_id().ok()?.shorten_or_id())),
    }
}

/// Vrai dès le premier changement HEAD↔index, index↔worktree ou fichier non suivi.
fn is_dirty(repo: &gix::Repository) -> bool {
    repo.status(gix::progress::Discard)
        .and_then(|status| status.into_iter(None))
        .is_ok_and(|mut changes| changes.any(|change| change.is_ok()))
}

/// Lance le thread de statut. Chaque envoi sur le canal retourné fixe les dossiers suivis et
/// déclenche un calcul immédiat dont tous les résultats sont envoyés ; ensuite, toutes les
/// `STATUS_TTL`, seuls les résultats qui ont changé le sont (`Event::Git`).
pub fn spawn_refresher(tx: Sender<Event>) -> Sender<Vec<PathBuf>> {
    let (requests, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("git-status".into())
        .spawn(move || refresh_loop(&rx, &tx))
        .expect("thread de statut git");
    requests
}

fn refresh_loop(rx: &Receiver<Vec<PathBuf>>, tx: &Sender<Event>) {
    let mut cwds: Vec<PathBuf> = Vec::new();
    let mut last: HashMap<PathBuf, Option<Repo>> = HashMap::new();
    loop {
        let forced = match rx.recv_timeout(STATUS_TTL) {
            Ok(request) => {
                // Les demandes accumulées pendant un calcul se résument à la dernière.
                cwds = rx.try_iter().last().unwrap_or(request);
                last.retain(|cwd, _| cwds.contains(cwd));
                true
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        for cwd in &cwds {
            let repo = inspect(cwd);
            if !forced && last.get(cwd) == Some(&repo) {
                continue;
            }
            last.insert(cwd.clone(), repo.clone());
            if tx.send(Event::Git(cwd.clone(), repo)).is_err() {
                return;
            }
        }
    }
}
