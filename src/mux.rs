//! Multiplexeurs (SPEC.md § Sessions headless et § rex) : une instance tourne dans un pane zellij
//! ou un block rex. Les actions sur une instance passent par le sien ; ce qu'xst ouvre (reprise,
//! arrière-plan) va dans le multiplexeur d'xst.

use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use crate::record::{Pane, Rex};
use crate::watch::Event;
use crate::{rex, zellij};

/// Multiplexeur où tourne xst.
pub enum Host {
    Zellij { session: String },
    Rex(Rex),
    Outside,
}

impl Host {
    /// zellij d'abord, comme pour une instance : un zellij lancé dans un terminal rex hérite des
    /// variables de rex.
    pub fn detect() -> Host {
        if let Ok(session) = std::env::var("ZELLIJ_SESSION_NAME") {
            return Host::Zellij { session };
        }
        match (std::env::var("REX_SESSION"), std::env::var("REX_BLOCK")) {
            (Ok(session), Ok(block)) => Host::Rex(Rex { session, block, server: std::env::var("REX_SERVER").ok() }),
            _ => Host::Outside,
        }
    }

    /// Session zellij d'xst.
    pub fn zellij(&self) -> Option<&str> {
        match self {
            Host::Zellij { session } => Some(session),
            _ => None,
        }
    }
}

/// Vérifie qu'une action (saut, fermeture, arrière-plan) peut atteindre `pane` depuis `host` : un
/// pane zellij depuis sa session zellij, un block rex depuis n'importe où.
pub fn check(host: &Host, pane: Option<Pane>) -> Result<(), String> {
    match pane {
        None => Err("Instance hors zellij et rex".into()),
        Some(Pane::Zellij(z)) => zellij::check_target(host.zellij(), z),
        Some(Pane::Rex(_)) => Ok(()),
    }
}

pub fn reachable(host: &Host, pane: Option<Pane>) -> bool {
    check(host, pane).is_ok()
}

/// Sessions à relire pour savoir quels panes sont headless : celle d'xst dans zellij, et les
/// sessions rex (serveur, session) des instances.
#[derive(Default, Clone)]
pub struct Scope {
    pub zellij: bool,
    pub rex: Vec<(Option<String>, String)>,
}

pub struct Mux {
    pub host: Host,
    /// Plugin zellij des sessions headless ; seulement avec xst dans zellij.
    plugin: Result<zellij::Plugin, String>,
    /// Demandes au thread qui relit les panes headless.
    refresh: Sender<()>,
    scope: Arc<Mutex<Scope>>,
}

impl Mux {
    pub fn new(home: &Path, tx: Sender<Event>) -> Mux {
        let host = Host::detect();
        let scope = Arc::new(Mutex::new(Scope::default()));
        let refresh = spawn_refresher(scope.clone(), tx);
        let plugin = match host {
            Host::Zellij { .. } => zellij::Plugin::install(&home.join(".cache/xst"), refresh.clone())
                .map_err(|err| format!("plugin xs-tui : {err}")),
            Host::Rex(_) => Err("xst dans rex".into()),
            Host::Outside => Err(OUTSIDE.into()),
        };
        Mux { host, plugin, refresh, scope }
    }

    /// Vrai si xst peut reprendre une session en arrière-plan ; sinon la raison.
    pub fn background_ready(&self) -> Result<(), String> {
        match self.host {
            Host::Rex(_) => Ok(()),
            _ => self.plugin.as_ref().map(drop).map_err(Clone::clone),
        }
    }

    /// Relit les panes headless de `scope`.
    pub fn refresh(&self, scope: Scope) {
        if let Ok(mut current) = self.scope.lock() {
            *current = scope;
        }
        let _ = self.refresh.send(());
    }

    pub fn focus(&self, pane: Option<Pane>) -> Result<(), String> {
        check(&self.host, pane)?;
        match pane {
            Some(Pane::Zellij(z)) => zellij::focus(self.host.zellij(), z),
            Some(Pane::Rex(r)) => rex::focus(r),
            None => Ok(()),
        }
    }

    pub fn close(&self, pane: Option<Pane>) -> Result<(), String> {
        check(&self.host, pane)?;
        match pane {
            Some(Pane::Zellij(z)) => zellij::close(self.host.zellij(), z),
            Some(Pane::Rex(r)) => rex::close(r),
            None => Ok(()),
        }
    }

    /// Reprend la session `id` dans `cwd`, dans un nouvel onglet ou une nouvelle fenêtre d'xst.
    pub fn resume(&self, id: &str, cwd: &Path) -> Result<(), String> {
        let script = resume_script(id, cwd);
        match &self.host {
            Host::Zellij { .. } => zellij::resume(&script, cwd),
            Host::Rex(own) => rex::resume(own, &script, cwd),
            Host::Outside => Err("xst hors zellij et rex : reprise impossible".into()),
        }
    }

    /// Reprend la session `id` dans `cwd` en arrière-plan : pane headless de zellij par le plugin,
    /// dont la réponse arrive plus tard par `tx`, ou block détaché de la session rex d'xst, à la
    /// taille du terminal d'xst.
    pub fn background(&self, id: &str, cwd: &Path, tx: &Sender<Event>) -> Result<(), String> {
        let script = resume_script(id, cwd);
        match &self.host {
            Host::Rex(own) => {
                let size = crossterm::terminal::size().map_err(|err| format!("taille du terminal : {err}"))?;
                rex::spawn(own, &script, cwd, size)?;
                let _ = self.refresh.send(());
                Ok(())
            }
            _ => {
                self.plugin.as_ref().map_err(Clone::clone)?.send(zellij::Headless::Spawn { script: &script, cwd }, tx);
                Ok(())
            }
        }
    }

    /// Passe `pane` en arrière-plan.
    pub fn hide(&self, pane: Option<Pane>, tx: &Sender<Event>) -> Result<(), String> {
        check(&self.host, pane)?;
        match pane {
            Some(Pane::Zellij(z)) => self.plugin_send(zellij::Headless::Hide(&z.pane_id), tx),
            Some(Pane::Rex(r)) => self.rex_then_refresh(rex::hide(r)),
            None => Ok(()),
        }
    }

    /// Sort `pane`, headless, dans un nouvel onglet ou une nouvelle fenêtre, qui prend le focus.
    pub fn show(&self, pane: Option<Pane>, tx: &Sender<Event>) -> Result<(), String> {
        check(&self.host, pane)?;
        match pane {
            Some(Pane::Zellij(z)) => self.plugin_send(zellij::Headless::Show(&z.pane_id), tx),
            Some(Pane::Rex(r)) => self.rex_then_refresh(rex::show(r)),
            None => Ok(()),
        }
    }

    fn plugin_send(&self, command: zellij::Headless, tx: &Sender<Event>) -> Result<(), String> {
        self.plugin.as_ref().map_err(Clone::clone)?.send(command, tx);
        Ok(())
    }

    /// rex applique ses actions avant de répondre : les blocks détachés se relisent aussitôt.
    fn rex_then_refresh(&self, result: Result<(), String>) -> Result<(), String> {
        let _ = self.refresh.send(());
        result
    }

    /// Nomme l'onglet ou la fenêtre d'xst.
    pub fn rename_own_tab(&self) -> Result<(), String> {
        match &self.host {
            Host::Zellij { .. } => match std::env::var("ZELLIJ_PANE_ID") {
                Ok(pane) => zellij::rename_tab_of(&pane, zellij::TAB_NAME),
                Err(_) => Ok(()),
            },
            Host::Rex(own) => rex::rename_window_of(own, zellij::TAB_NAME),
            Host::Outside => Ok(()),
        }
    }
}

const OUTSIDE: &str = "xst hors zellij et rex : arrière-plan impossible";

/// Thread qui relit les panes headless de la portée courante à chaque demande ; les demandes en
/// rafale sont fusionnées en une seule relecture. Sans réponse de zellij, les derniers panes
/// connus restent affichés.
fn spawn_refresher(scope: Arc<Mutex<Scope>>, tx: Sender<Event>) -> Sender<()> {
    let (requests, rx) = mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("headless".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                while rx.try_recv().is_ok() {}
                let Ok(scope) = scope.lock().map(|s| s.clone()) else { return };
                if scope.zellij
                    && let Ok(panes) = zellij::headless_panes()
                    && tx.send(Event::Headless(panes)).is_err()
                {
                    return;
                }
                let detached = if scope.rex.is_empty() { HashSet::new() } else { rex::detached(&scope.rex) };
                if tx.send(Event::Detached(detached)).is_err() {
                    return;
                }
            }
        })
        .expect("thread des panes headless");
    requests
}

/// Script fish qui reprend la session `id` dans `cwd` ; fish reste ouvert après la sortie d'omp.
///
/// fish n'active pas le contrôle des tâches pour `-C` : sans `status job-control full`, omp
/// resterait dans le groupe de processus de fish. Son Ctrl+Z (`SIGSTOP` au groupe) gèlerait alors
/// fish avec lui, et fermer le pane laisserait omp arrêté pour toujours.
fn resume_script(id: &str, cwd: &Path) -> String {
    format!("status job-control full; cd {}; and omp --resume {id}", fish_quote(cwd))
}

/// Chaîne fish entre apostrophes : seuls `\` et `'` s'y échappent.
fn fish_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Zellij;

    #[test]
    fn fish_quote_escapes_backslashes_and_quotes() {
        assert_eq!(fish_quote(Path::new(r"/tmp/l'a\b")), r"'/tmp/l\'a\\b'");
    }

    #[test]
    fn zellij_panes_need_the_same_session_but_rex_blocks_do_not() {
        let z = Zellij { session: "m1".into(), pane_id: "8".into() };
        let r = Rex { session: "session:a".into(), block: "block:b".into(), server: None };
        let in_m1 = Host::Zellij { session: "m1".into() };
        assert!(reachable(&in_m1, Some(Pane::Zellij(&z))));
        assert!(!reachable(&Host::Zellij { session: "m2".into() }, Some(Pane::Zellij(&z))));
        assert!(!reachable(&Host::Outside, Some(Pane::Zellij(&z))));
        assert!(reachable(&Host::Outside, Some(Pane::Rex(&r))));
        assert!(reachable(&in_m1, Some(Pane::Rex(&r))));
        assert!(!reachable(&in_m1, None));
    }
}
