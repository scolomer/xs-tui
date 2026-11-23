//! Copie de la ligne sélectionnée (SPEC.md § Copie de session). Pour une instance omp, un thread
//! relit l'écran visible de son pane toutes les `POLL` (`zellij action dump-screen --ansi`, ou
//! `rex capture --format=vt`), et l'envoie converti en texte ratatui quand il a changé. Pour un
//! thread Capy, un thread suit sa conversation (`conversation.rs`).
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

use crate::capy::Api;
use crate::conversation::{self, Conversation};
use crate::record::{Pane, Rex};
use crate::rex;
use crate::watch::Event;

/// Intervalle entre deux relectures de l'écran copié.
const POLL: Duration = Duration::from_millis(250);

/// Contenu de la zone de copie.
pub enum View {
    /// Copie lancée, premier écran pas encore lu.
    Waiting,
    Screen(Text<'static>),
    Conversation(Conversation),
    Message(String),
}

/// Ce que le thread de copie envoie à la boucle principale.
pub enum Update {
    Screen(Text<'static>),
    Conversation(Conversation),
    /// Fin de la copie, avec le message à afficher.
    Failed(String),
}

/// Ce que suit la copie.
#[derive(PartialEq, Eq)]
enum Target {
    /// (session zellij, id du pane).
    Zellij(String, String),
    Rex(Rex),
    /// Id du thread Capy.
    Capy(String),
}

pub struct Mirror {
    /// Cible copiée. Gardée après un échec pour ne pas relancer la copie avant que la cible change.
    target: Option<Target>,
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

    /// Copie le pane `pane` ; rien si c'est déjà la cible.
    pub fn follow(&mut self, pane: Pane) {
        match pane {
            Pane::Zellij(z) => {
                let target = Target::Zellij(z.session.clone(), z.pane_id.clone());
                let (session, pane) = (z.session.clone(), format!("terminal_{}", z.pane_id));
                self.start(target, "zellij-dump-screen", move |stop, generation, tx| {
                    poll(|| dump(&session, &pane), &stop, generation, &tx)
                });
            }
            Pane::Rex(r) => {
                let block = r.clone();
                self.start(Target::Rex(r.clone()), "rex-capture", move |stop, generation, tx| {
                    let screen = || rex::screen(&block).map_err(|m| if m == "rex introuvable" { m } else { format!("Copie interrompue : {m}") });
                    poll(screen, &stop, generation, &tx)
                });
            }
        }
    }

    /// Suit la conversation du thread Capy `thread` ; rien si c'est déjà la cible. Le flux se ferme
    /// quand la cible change ou que la copie est fermée.
    pub fn follow_capy(&mut self, api: &Api, thread: &str) {
        let (api, id) = (api.clone(), thread.to_string());
        self.start(Target::Capy(id.clone()), "capy-stream", move |stop, generation, tx| {
            conversation::follow(&api, &id, &stop, generation, &tx)
        });
    }

    /// Arrête la copie en cours et lance `run` dans un thread pour `target`.
    fn start<F>(&mut self, target: Target, name: &str, run: F)
    where
        F: FnOnce(Arc<AtomicBool>, u64, Sender<Event>) + Send + 'static,
    {
        if self.target.as_ref() == Some(&target) {
            return;
        }
        self.stop.store(true, Ordering::Relaxed);
        self.stop = Arc::new(AtomicBool::new(false));
        self.target = Some(target);
        self.generation += 1;
        let (stop, generation, tx) = (self.stop.clone(), self.generation, self.tx.clone());
        self.view = match std::thread::Builder::new().name(name.into()).spawn(move || run(stop, generation, tx)) {
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
            Update::Conversation(conversation) => View::Conversation(conversation),
            Update::Failed(message) => View::Message(message),
        };
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Relit l'écran par `read` jusqu'à `stop` ou au premier échec ; n'envoie que les écrans qui changent.
fn poll(read: impl Fn() -> Result<Vec<u8>, String>, stop: &AtomicBool, generation: u64, tx: &Sender<Event>) {
    let mut last: Vec<u8> = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let update = match read() {
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
