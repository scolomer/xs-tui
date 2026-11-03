//! Branche git d'un dossier et compteurs de statut, lus avec gix (aucun process git lancé).
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
    pub counts: Counts,
    /// Worktree lié rangé à côté du dépôt principal sous le nom `<dépôt>.<suffixe>` : (racine du
    /// worktree, nom du dossier du dépôt principal), à afficher à la place du nom de la racine.
    pub alias: Option<(PathBuf, String)>,
}

/// Compteurs du segment git d'omp, au sens de `git status --porcelain` : un fichier indexé puis
/// modifié compte dans `staged` et dans `unstaged` ; un dossier non suivi compte pour un.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Counts {
    /// HEAD ↔ index (colonne X).
    pub staged: usize,
    /// Index ↔ worktree (colonne Y), conflits compris.
    pub unstaged: usize,
    /// Entrées `??`.
    pub untracked: usize,
}

impl Counts {
    pub fn dirty(&self) -> bool {
        self.staged + self.unstaged + self.untracked > 0
    }
}

/// Dépôt contenant `cwd`, ou `None` hors dépôt.
pub fn inspect(cwd: &Path) -> Option<Repo> {
    let repo = gix::discover(cwd).ok()?;
    Some(Repo {
        git_dir: repo.git_dir().to_path_buf(),
        label: head_label(&repo),
        counts: counts(&repo),
        alias: worktree_alias(&repo),
    })
}

/// Nom du dépôt principal pour un worktree lié nommé `<dépôt principal>.<suffixe>` : le suffixe
/// reprend la branche (`feature-sc-134990-…`), déjà affichée dans le segment branche.
fn worktree_alias(repo: &gix::Repository) -> Option<(PathBuf, String)> {
    // `commondir` d'un worktree vaut souvent `../..` : chemin à résoudre avant d'en prendre le parent.
    let common = repo.common_dir().canonicalize().ok()?;
    if common == repo.git_dir().canonicalize().ok()? {
        return None;
    }
    let root = repo.workdir()?;
    let main = common.parent()?.file_name()?.to_str()?;
    let suffix = root.file_name()?.to_str()?.strip_prefix(main)?.strip_prefix('.')?;
    (!suffix.is_empty()).then(|| (root.to_path_buf(), main.to_string()))
}

/// Équivalent de `git branch --show-current`, ou `@<git rev-parse --short HEAD>` si HEAD est détaché.
/// Recopié de `head_label` d'omp-sessions, sans l'abréviation du nom de branche.
fn head_label(repo: &gix::Repository) -> Option<String> {
    match repo.head_name().ok()? {
        Some(name) => Some(name.shorten().to_string()),
        None => Some(format!("@{}", repo.head_id().ok()?.shorten_or_id())),
    }
}

/// Compteurs de statut ; zéro partout si le statut ne peut pas être calculé.
fn counts(repo: &gix::Repository) -> Counts {
    use gix::dir::entry::Status as DirStatus;
    use gix::status::Item;
    use gix::status::index_worktree::Item as Worktree;
    use gix::status::plumbing::index_as_worktree::EntryStatus;

    let mut counts = Counts::default();
    let Ok(changes) = repo.status(gix::progress::Discard).and_then(|status| status.into_iter(None)) else {
        return counts;
    };
    for change in changes.flatten() {
        match change {
            Item::TreeIndex(_) => counts.staged += 1,
            Item::IndexWorktree(Worktree::Modification { status, .. }) => {
                // Stat seule différente, contenu identique : pas une modification pour git.
                if !matches!(status, EntryStatus::NeedsUpdate(_)) {
                    counts.unstaged += 1;
                }
            }
            Item::IndexWorktree(Worktree::DirectoryContents { entry, .. }) => {
                if entry.status == DirStatus::Untracked {
                    counts.untracked += 1;
                }
            }
            // Renommages détectés dans le worktree : désactivés par défaut, comptés comme git
            // le ferait sans détection (une suppression et un fichier non suivi).
            Item::IndexWorktree(Worktree::Rewrite { .. }) => {
                counts.unstaged += 1;
                counts.untracked += 1;
            }
        }
    }
    counts
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
