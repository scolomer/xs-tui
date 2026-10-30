//! Branche git d'un dossier, lue avec gix (aucun process git lancé).

use std::path::{Path, PathBuf};

pub struct Repo {
    /// Gitdir propre au worktree (`.git/worktrees/<n>` pour un worktree) : son HEAD est surveillé.
    pub git_dir: PathBuf,
    pub label: Option<String>,
}

/// Dépôt contenant `cwd`, ou `None` hors dépôt.
pub fn inspect(cwd: &Path) -> Option<Repo> {
    let repo = gix::discover(cwd).ok()?;
    Some(Repo { git_dir: repo.git_dir().to_path_buf(), label: head_label(&repo) })
}

/// Équivalent de `git branch --show-current`, ou `@<git rev-parse --short HEAD>` si HEAD est détaché.
/// Recopié de `head_label` d'omp-sessions, sans l'abréviation du nom de branche.
fn head_label(repo: &gix::Repository) -> Option<String> {
    match repo.head_name().ok()? {
        Some(name) => Some(name.shorten().to_string()),
        None => Some(format!("@{}", repo.head_id().ok()?.shorten_or_id())),
    }
}
