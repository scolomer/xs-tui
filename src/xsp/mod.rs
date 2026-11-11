//! Client xs-parley natif : xst est le session-manager (rang `M`) des parleys de la machine et
//! reprend en headless les sessions omp qu'un master réveille par un `spawn` (SPEC.md
//! § Session-manager xs-parley).
//!
//! `vectors/` est une copie de xs-parley/doc/protocol/vectors, à resynchroniser après `gen-vectors`.

mod cert;
mod codec;
mod envelope;
mod keys;
mod manager;
mod relay;
mod store;

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};

use crate::watch::Event;
use manager::{Input, State};
use relay::Connection;

pub use store::root;

/// Demande de réveil de la session omp `session`, transmise par le master `from_alias`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnRequest {
    pub id: u64,
    pub session: String,
    pub from_alias: String,
    pub parley_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagerStatus {
    /// Parleys rejointes ou en cours de connexion.
    pub parleys: usize,
    pub problem: Option<String>,
}

#[derive(Debug)]
pub enum XspEvent {
    Spawn(SpawnRequest),
    Status(ManagerStatus),
}

#[derive(Debug)]
pub enum Command {
    /// Résultat de la reprise : `Ok(None)` instance publiée, `Ok(Some(note))` rien à faire
    /// (`déjà ouverte`), `Err(raison)`.
    Spawned { id: u64, result: Result<Option<String>, String> },
    /// `parleys/` a changé : rejoindre, quitter.
    Reload,
}

pub struct Manager {
    cmd: Sender<Input>,
}

impl Manager {
    pub fn send(&self, command: Command) {
        let _ = self.cmd.send(Input::Cmd(command));
    }
}

/// Charge ou crée l'identité du manager dans `root` et lance son thread ; une erreur signifie pas
/// de manager.
pub fn start(root: PathBuf, tx: Sender<Event>) -> Result<Manager, String> {
    let identity = store::load_or_create_identity(&root)?;
    let (cmd, inputs) = mpsc::channel::<Input>();
    let conn_events = cmd.clone();
    let state = State::new(
        root,
        identity,
        Box::new(move |event| {
            let _ = tx.send(Event::Xsp(event));
        }),
        Box::new(move |endpoint| Connection::spawn(endpoint.to_string(), conn_events.clone())),
    );
    std::thread::Builder::new()
        .name("xsp-manager".into())
        .spawn(move || state.run(&inputs))
        .map_err(|e| format!("thread du manager : {e}"))?;
    Ok(Manager { cmd })
}

/// Dossier `parleys/` surveillé par xst.
pub fn parleys_dir(root: &Path) -> PathBuf {
    root.join("parleys")
}
