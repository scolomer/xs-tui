//! Événements de la boucle principale : fichiers publiés, épingles, gitdirs, statut git, mort d'un
//! pid, terminal.
//!
//! Une seule instance `notify` (FSEvents sur macOS, inotify sur Linux) surveille le dossier
//! d'exécution récursivement, le dossier agent d'omp (pour `session-pins.json`) et les gitdirs des
//! instances, sans récursion.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use crossterm::event::{self as term, KeyEvent, KeyEventKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::pins::PINS_FILE;

pub enum Event {
    /// Un fichier publié a changé : relire le dossier d'exécution.
    Fs,
    /// `HEAD` ou l'index d'un gitdir a changé : recalculer branches et statut.
    Head,
    /// `session-pins.json` a changé : relire les sessions épinglées.
    Pins,
    /// Résultat du thread de statut pour un `cwd` (`None` hors dépôt).
    Git(PathBuf, Option<crate::git::Repo>),
    ProcExit(i32),
    /// Mise à jour de l'abonnement de copie numéro `.0`.
    Pane(u64, crate::mirror::Update),
    /// Consommation lue par `omp usage`, ou message d'erreur.
    Usage(Result<Vec<crate::usage::Account>, String>),
    /// Message d'un thread pour la barre du bas (plugin xs-tui).
    Notice(String),
    /// Ids des panes headless de la session zellij courante.
    Headless(std::collections::HashSet<String>),
    Key(KeyEvent),
    /// Texte collé (bracketed paste).
    Paste(String),
    Resize,
}

/// Surveille `run_dir` (créé au besoin) et `agent_dir`. Les événements arrêtent d'arriver quand le
/// watcher est libéré.
pub fn start(run_dir: &Path, agent_dir: &Path, tx: Sender<Event>) -> notify::Result<RecommendedWatcher> {
    std::fs::create_dir_all(run_dir)?;
    // FSEvents rapporte des chemins canoniques : comparer avec la forme canonique du dossier.
    let root: PathBuf = run_dir.canonicalize()?;
    let prefix = root.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        let head = event.paths.iter().any(|p| p.file_name().is_some_and(|n| n == "HEAD" || n == "index"));
        let pins = event.paths.iter().any(|p| p.file_name().is_some_and(|n| n == PINS_FILE));
        let published = event.paths.iter().any(|p| p.starts_with(&prefix) && !p.extension().is_some_and(|e| e == "tmp"));
        if head {
            let _ = tx.send(Event::Head);
        }
        if pins {
            let _ = tx.send(Event::Pins);
        }
        if published {
            let _ = tx.send(Event::Fs);
        }
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;
    watcher.watch(agent_dir, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

/// Ajoute le gitdir d'une instance pour suivre son HEAD et son index.
pub fn watch_git_dir(watcher: &mut RecommendedWatcher, git_dir: &Path) -> notify::Result<()> {
    watcher.watch(git_dir, RecursiveMode::NonRecursive)
}

/// Thread de lecture du terminal : touches, textes collés et redimensionnements.
pub fn spawn_terminal_reader(tx: Sender<Event>) {
    std::thread::Builder::new()
        .name("terminal".into())
        .spawn(move || {
            while let Ok(event) = term::read() {
                let sent = match event {
                    term::Event::Key(key) if key.kind == KeyEventKind::Press => tx.send(Event::Key(key)),
                    term::Event::Paste(text) => tx.send(Event::Paste(text)),
                    term::Event::Resize(..) => tx.send(Event::Resize),
                    _ => Ok(()),
                };
                if sent.is_err() {
                    return;
                }
            }
        })
        .expect("thread de lecture du terminal");
}
