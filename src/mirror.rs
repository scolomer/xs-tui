//! Copie d'un pane zellij (SPEC.md § Copie de session) : un thread relit l'écran visible du pane
//! avec `zellij action dump-screen --ansi` toutes les `POLL`, et l'envoie converti en texte
//! ratatui quand il a changé.
//!
//! Relecture à intervalle fixe : `zellij subscribe` ne diffuse rien pour un pane dont l'onglet n'est
//! pas affiché, puisque zellij ne le redessine pas.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use ansi_to_tui::IntoText;
use ratatui::text::Text;

use crate::watch::Event;

/// Intervalle entre deux relectures de l'écran copié.
const POLL: Duration = Duration::from_millis(250);

/// Contenu de la zone de copie.
pub enum View {
    /// Copie lancée, premier écran pas encore lu.
    Waiting,
    Screen(Text<'static>),
    Message(String),
}

/// Ce que le thread de copie envoie à la boucle principale.
pub enum Update {
    Screen(Text<'static>),
    /// Fin de la copie, avec le message à afficher.
    Failed(String),
}

pub struct Mirror {
    /// Pane copié : (session zellij, id du pane). Gardé après un échec pour ne pas relancer la
    /// copie avant que la cible change.
    target: Option<(String, String)>,
    /// Arrête le thread de la copie en cours.
    stop: Arc<AtomicBool>,
    /// Numéro de la copie en cours : les mises à jour d'une copie remplacée sont ignorées.
    generation: u64,
    pub view: View,
    tx: Sender<Event>,
}

impl Mirror {
    pub fn new(tx: Sender<Event>) -> Self {
        Mirror { target: None, stop: Arc::new(AtomicBool::new(true)), generation: 0, view: View::Waiting, tx }
    }

    /// Copie le pane `pane_id` de la session zellij `session` ; rien si c'est déjà la cible.
    pub fn follow(&mut self, session: &str, pane_id: &str) {
        if self.target.as_ref().is_some_and(|(s, p)| s == session && p == pane_id) {
            return;
        }
        self.stop.store(true, Ordering::Relaxed);
        self.stop = Arc::new(AtomicBool::new(false));
        self.target = Some((session.to_string(), pane_id.to_string()));
        self.generation += 1;
        let (session, pane, stop, generation, tx) =
            (session.to_string(), format!("terminal_{pane_id}"), self.stop.clone(), self.generation, self.tx.clone());
        self.view = match std::thread::Builder::new()
            .name("zellij-dump-screen".into())
            .spawn(move || poll(&session, &pane, &stop, generation, &tx))
        {
            Ok(_) => View::Waiting,
            Err(err) => View::Message(format!("Copie interrompue : {err}")),
        };
    }

    /// Aucun pane à copier : affiche `message` à la place.
    pub fn show(&mut self, message: &str) {
        self.stop.store(true, Ordering::Relaxed);
        self.target = None;
        if !matches!(&self.view, View::Message(m) if m == message) {
            self.view = View::Message(message.to_string());
        }
    }

    pub fn apply(&mut self, generation: u64, update: Update) {
        if generation != self.generation {
            return;
        }
        self.view = match update {
            Update::Screen(text) => View::Screen(text),
            Update::Failed(message) => View::Message(message),
        };
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Relit l'écran de `pane` jusqu'à `stop` ou au premier échec ; n'envoie que les écrans qui changent.
fn poll(session: &str, pane: &str, stop: &AtomicBool, generation: u64, tx: &Sender<Event>) {
    let mut last: Vec<u8> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let update = match dump(session, pane) {
            Ok(screen) if screen == last => None,
            Ok(screen) => {
                let text = screen.into_text().unwrap_or_default();
                last = screen;
                Some(Update::Screen(text))
            }
            Err(message) => Some(Update::Failed(message)),
        };
        let failed = matches!(update, Some(Update::Failed(_)));
        if let Some(update) = update
            && tx.send(Event::Pane(generation, update)).is_err()
        {
            return;
        }
        if failed {
            return;
        }
        std::thread::sleep(POLL);
    }
}

/// Écran visible du pane, en ANSI ; message d'erreur à afficher sinon.
fn dump(session: &str, pane: &str) -> Result<Vec<u8>, String> {
    let out = Command::new("zellij")
        .args(["--session", session, "action", "dump-screen", "--pane-id", pane, "--ansi"])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => "zellij introuvable".to_string(),
            _ => format!("Copie interrompue : {err}"),
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("échec de zellij");
        return Err(format!("Copie interrompue : {reason}"));
    }
    // Un pane absent ne fait pas échouer zellij : il renvoie une ligne vide. Un pane omp affiche
    // toujours au moins son statut.
    if out.stdout.iter().all(u8::is_ascii_whitespace) {
        return Err("Copie interrompue : pane fermé".to_string());
    }
    Ok(out.stdout)
}
