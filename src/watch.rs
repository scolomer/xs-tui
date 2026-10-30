//! Événements de la boucle principale : fichiers publiés, HEAD git, mort d'un pid, terminal.
//!
//! Une seule instance `notify` (FSEvents sur macOS, inotify sur Linux) surveille le dossier
//! d'exécution récursivement et les gitdirs des instances, sans récursion.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use crossterm::event::{self as term, KeyEvent, KeyEventKind};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};

pub enum Event {
    /// Un fichier publié a changé : relire le dossier d'exécution.
    Fs,
    /// Un HEAD git a changé : relire les branches.
    Head,
    ProcExit(i32),
    Key(KeyEvent),
    Resize,
}

/// Surveille `run_dir` (créé au besoin). Les événements arrêtent d'arriver quand le watcher est libéré.
pub fn start(run_dir: &Path, tx: Sender<Event>) -> notify::Result<RecommendedWatcher> {
    std::fs::create_dir_all(run_dir)?;
    // FSEvents rapporte des chemins canoniques : comparer avec la forme canonique du dossier.
    let root: PathBuf = run_dir.canonicalize()?;
    let prefix = root.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(event) = res else { return };
        let head = event.paths.iter().any(|p| p.file_name().is_some_and(|n| n == "HEAD"));
        let published = event.paths.iter().any(|p| p.starts_with(&prefix) && !p.extension().is_some_and(|e| e == "tmp"));
        if head {
            let _ = tx.send(Event::Head);
        }
        if published {
            let _ = tx.send(Event::Fs);
        }
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;
    Ok(watcher)
}

/// Ajoute le gitdir d'une instance pour suivre son HEAD.
pub fn watch_git_dir(watcher: &mut RecommendedWatcher, git_dir: &Path) -> notify::Result<()> {
    watcher.watch(git_dir, RecursiveMode::NonRecursive)
}

/// Thread de lecture du terminal : touches et redimensionnements.
pub fn spawn_terminal_reader(tx: Sender<Event>) {
    std::thread::Builder::new()
        .name("terminal".into())
        .spawn(move || {
            while let Ok(event) = term::read() {
                let sent = match event {
                    term::Event::Key(key) if key.kind == KeyEventKind::Press => tx.send(Event::Key(key)),
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
