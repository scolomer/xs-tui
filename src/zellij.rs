//! Actions zellij : saut vers le pane d'une instance (`focus-pane-id`), reprise d'une session
//! dormante dans un nouvel onglet (`new-tab`), sessions headless par le plugin xs-tui
//! (`zellij pipe`, SPEC.md § Sessions headless) et nom de l'onglet d'xst.

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::record::Zellij;
use crate::watch::Event;

/// Plugin zellij xs-tui (`plugin/`), compilé par `build.rs`.
const PLUGIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/xs-tui.wasm"));
/// Au-delà, le plugin attend sans doute l'accord de ses permissions, demandé à son premier lancement.
const PLUGIN_PATIENCE: Duration = Duration::from_secs(3);
/// Fin de l'attente d'une réponse du plugin.
const PLUGIN_TIMEOUT: Duration = Duration::from_secs(120);

/// Nom donné à l'onglet zellij d'xst.
pub const TAB_NAME: &str = "xs-tui";

/// Vrai si `focus` peut atteindre `target` depuis la session zellij `own`.
pub fn reachable(own: Option<&str>, target: Option<&Zellij>) -> bool {
    check_target(own, target).is_ok()
}

/// Focalise le pane `target` ; message d'erreur à afficher dans la barre sinon.
pub fn focus(own: Option<&str>, target: Option<&Zellij>) -> Result<(), String> {
    let target = check_target(own, target)?;
    run(Command::new("zellij").args(["action", "focus-pane-id", &format!("terminal_{}", target.pane_id)]))
}

/// Vérifie qu'une action (saut, fermeture, plugin) peut atteindre `target` depuis la session
/// zellij `own`.
pub fn check_target<'a>(own: Option<&str>, target: Option<&'a Zellij>) -> Result<&'a Zellij, String> {
    let target = target.ok_or("Instance hors zellij")?;
    if own != Some(target.session.as_str()) {
        return Err(format!("Instance dans la session zellij “{}”", target.session));
    }
    Ok(target)
}

/// Ferme le pane `target`. zellij envoie `SIGHUP` à omp, qui fait sa fermeture normale : session
/// sauvegardée, dossier publié supprimé par l'extension.
pub fn close(own: Option<&str>, target: Option<&Zellij>) -> Result<(), String> {
    let target = check_target(own, target)?;
    run(Command::new("zellij").args(["action", "close-pane", "--pane-id", &format!("terminal_{}", target.pane_id)]))
}

/// Reprend la session `id` dans un nouvel onglet de la session zellij courante, ouvert dans `cwd`,
/// comme `zellij-resume.sh` d'omp-sessions.
pub fn resume(own: Option<&str>, id: &str, cwd: &Path) -> Result<(), String> {
    if own.is_none() {
        return Err("xst hors zellij : reprise impossible".into());
    }
    run(Command::new("zellij").args(["action", "new-tab", "--cwd"]).arg(cwd).args(["--", "fish", "-C", &resume_script(id, cwd)]))
}

/// Script fish qui reprend la session `id` dans `cwd` ; fish reste ouvert après la sortie d'omp.
///
/// fish n'active pas le contrôle des tâches pour `-C` : sans `status job-control full`, omp
/// resterait dans le groupe de processus de fish. Son Ctrl+Z (`SIGSTOP` au groupe) gèlerait alors
/// fish avec lui, et fermer le pane laisserait omp arrêté pour toujours.
fn resume_script(id: &str, cwd: &Path) -> String {
    format!("status job-control full; cd {}; and omp --resume {id}", fish_quote(cwd))
}

/// Commande du plugin xs-tui (`plugin/src/main.rs`).
pub enum Headless<'a> {
    /// Reprend la session `id`, ouverte dans `cwd`, dans un pane headless.
    Resume { id: &'a str, cwd: &'a Path },
    /// Sort le pane headless dans un nouvel onglet, qui prend le focus.
    Show(&'a str),
    /// Rend le pane headless.
    Hide(&'a str),
}

impl Headless<'_> {
    /// Contenu du pipe : le nom de la commande puis ses arguments, un par ligne.
    fn payload(&self) -> String {
        match self {
            Headless::Resume { id, cwd } => {
                ["spawn", &cwd.to_string_lossy(), "fish", "-C", &resume_script(id, cwd)].join("\n")
            }
            Headless::Show(pane) => format!("show\n{pane}"),
            Headless::Hide(pane) => format!("hide\n{pane}"),
        }
    }
}

/// Plugin xs-tui installé, appelé par `zellij pipe` ; zellij le lance au premier appel.
pub struct Plugin {
    url: String,
    /// Demandes au thread qui relit les panes headless.
    refresh: Sender<()>,
}

impl Plugin {
    /// Écrit le plugin dans `dir` sous un nom qui dépend de son contenu, s'il n'y est pas déjà, et
    /// supprime les versions précédentes. zellij garde en mémoire le code d'un plugin par chemin :
    /// un nouveau nom fait charger la nouvelle version, au prix d'une nouvelle demande de
    /// permissions.
    pub fn install(dir: &Path, refresh: Sender<()>) -> std::io::Result<Plugin> {
        let name = format!("xs-tui-{}.wasm", env!("XS_TUI_PLUGIN_HASH"));
        let path = dir.join(&name);
        if !path.exists() {
            std::fs::create_dir_all(dir)?;
            for entry in std::fs::read_dir(dir)?.flatten() {
                let old = entry.file_name();
                let old = old.to_string_lossy();
                if old.starts_with("xs-tui-") && old.ends_with(".wasm") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            let tmp = dir.join(format!(".{name}.tmp"));
            std::fs::write(&tmp, PLUGIN)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(Plugin { url: format!("file:{}", path.display()), refresh })
    }

    /// Envoie `command` au plugin dans un thread : les erreurs arrivent par `tx`, et les panes
    /// headless sont relus une fois la commande faite.
    pub fn send(&self, command: Headless, tx: &Sender<Event>) {
        let payload = command.payload();
        let url = self.url.clone();
        let refresh = self.refresh.clone();
        let tx = tx.clone();
        std::thread::Builder::new()
            .name("xs-tui-plugin".into())
            .spawn(move || {
                if let Err(message) = pipe(&url, &payload, &tx) {
                    let _ = tx.send(Event::Notice(message));
                }
                // zellij applique les actions du plugin juste après sa réponse.
                for delay in [200, 800] {
                    std::thread::sleep(Duration::from_millis(delay));
                    let _ = refresh.send(());
                }
            })
            .expect("thread du plugin");
    }
}

/// Envoie `payload` au plugin et attend sa réponse, `ok` ou un message d'erreur.
fn pipe(url: &str, payload: &str, tx: &Sender<Event>) -> Result<(), String> {
    let mut child = Command::new("zellij")
        .args(["pipe", "--plugin", url, "--name", "xs-tui", "--", payload])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("zellij : {err}"))?;
    let start = Instant::now();
    let mut waiting_permissions = false;
    loop {
        if child.try_wait().map_err(|err| format!("zellij pipe : {err}"))?.is_some() {
            break;
        }
        let elapsed = start.elapsed();
        if !waiting_permissions && elapsed > PLUGIN_PATIENCE {
            waiting_permissions = true;
            reveal_plugin(url);
            let _ = tx.send(Event::Notice("xs-tui attend ses permissions : y dans son pane pour accepter".into()));
        }
        if elapsed > PLUGIN_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            // Le plugin garde la commande et l'exécute à l'accord de ses permissions, sans jamais
            // répondre au pipe qui l'attendait.
            return if waiting_permissions { Ok(()) } else { Err("xs-tui : pas de réponse du plugin".into()) };
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut reply = String::new();
    if let Some(mut out) = child.stdout.take() {
        let _ = out.read_to_string(&mut reply);
    }
    match reply.trim() {
        "ok" => Ok(()),
        "" => Err("xs-tui : pas de réponse du plugin".into()),
        other => Err(format!("xs-tui : {other}")),
    }
}

/// Thread qui relit les panes headless de la session zellij courante à chaque demande ; les
/// demandes en rafale sont fusionnées en une seule relecture.
pub fn spawn_headless_refresher(tx: Sender<Event>) -> Sender<()> {
    let (requests, rx) = mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("headless".into())
        .spawn(move || {
            while rx.recv().is_ok() {
                while rx.try_recv().is_ok() {}
                // Sans réponse de zellij, les derniers panes connus restent affichés.
                if let Ok(panes) = list_panes()
                    && tx.send(Event::Headless(headless_panes(&panes))).is_err()
                {
                    return;
                }
            }
        })
        .expect("thread des panes headless");
    requests
}

/// Ids des panes de terminal « supprimés » par zellij : affichés dans aucun onglet.
fn headless_panes(panes: &[Pane]) -> HashSet<String> {
    panes.iter().filter(|p| !p.is_plugin && p.is_suppressed).map(|p| p.id.to_string()).collect()
}

/// Affiche et focalise le pane du plugin qui attend ses permissions : lancé par un pipe, il s'ouvre
/// dans la couche flottante de l'onglet, qui reste masquée si elle l'était.
fn reveal_plugin(url: &str) {
    let Ok(panes) = list_panes() else { return };
    let Some(pane) = panes.iter().find(|p| p.is_plugin && !p.is_suppressed && p.plugin_url.as_deref() == Some(url))
    else {
        return;
    };
    let _ = run(Command::new("zellij").args(["action", "show-floating-panes", "--tab-id", &pane.tab_id.to_string()]));
    let _ = run(Command::new("zellij").args(["action", "focus-pane-id", &format!("plugin_{}", pane.id)]));
}

#[derive(Deserialize)]
struct Pane {
    id: u32,
    is_plugin: bool,
    is_suppressed: bool,
    tab_id: u32,
    plugin_url: Option<String>,
}

fn list_panes() -> Result<Vec<Pane>, String> {
    let out = Command::new("zellij")
        .args(["action", "list-panes", "--tab", "--state", "--json"])
        .output()
        .map_err(|err| format!("zellij : {err}"))?;
    serde_json::from_slice(&out.stdout).map_err(|err| format!("zellij list-panes : {err}"))
}

/// Renomme l'onglet qui contient le pane `pane_id`. Par son id et non par `rename-tab`, qui vise
/// l'onglet qui a le focus.
pub fn rename_tab_of(pane_id: &str, name: &str) -> Result<(), String> {
    let panes = list_panes()?;
    let tab = panes
        .iter()
        .find(|p| !p.is_plugin && p.id.to_string() == pane_id)
        .ok_or_else(|| format!("zellij : pane {pane_id} introuvable"))?
        .tab_id;
    run(Command::new("zellij").args(["action", "rename-tab-by-id", &tab.to_string(), name]))
}

fn run(command: &mut Command) -> Result<(), String> {
    let out = command.output().map_err(|err| format!("zellij : {err}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(format!("zellij : {}", stderr.trim().chars().take(80).collect::<String>()))
}

/// Chaîne fish entre apostrophes : seuls `\` et `'` s'y échappent.
fn fish_quote(path: &Path) -> String {
    let raw = path.to_string_lossy();
    format!("'{}'", raw.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fish_quote_escapes_backslashes_and_quotes() {
        assert_eq!(fish_quote(Path::new(r"/tmp/l'a\b")), r"'/tmp/l\'a\\b'");
    }
}
