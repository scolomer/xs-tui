//! Actions zellij : saut vers le pane d'une instance (`focus-pane-id`), reprise d'une session
//! dormante dans un nouvel onglet (`new-tab`) et nom de l'onglet d'agz.

use std::path::Path;
use std::process::Command;

use serde::Deserialize;

use crate::record::Zellij;

/// Nom donné à l'onglet zellij d'agz.
pub const TAB_NAME: &str = "agentz";

/// Vrai si `focus` peut atteindre `target` depuis la session zellij `own`.
pub fn reachable(own: Option<&str>, target: Option<&Zellij>) -> bool {
    check(own, target).is_ok()
}

/// Focalise le pane `target` ; message d'erreur à afficher dans la barre sinon.
pub fn focus(own: Option<&str>, target: Option<&Zellij>) -> Result<(), String> {
    let target = check(own, target)?;
    run(Command::new("zellij").args(["action", "focus-pane-id", &format!("terminal_{}", target.pane_id)]))
}

/// Reprend la session `id` dans un nouvel onglet de la session zellij courante, ouvert dans `cwd`,
/// comme `zellij-resume.sh` d'omp-sessions : fish reste ouvert après la sortie d'omp.
pub fn resume(own: Option<&str>, id: &str, cwd: &Path) -> Result<(), String> {
    if own.is_none() {
        return Err("agz hors zellij : reprise impossible".into());
    }
    let script = format!("cd {}; and omp --resume {id}", fish_quote(cwd));
    run(Command::new("zellij").args(["action", "new-tab", "--cwd"]).arg(cwd).args(["--", "fish", "-C", &script]))
}

#[derive(Deserialize)]
struct Pane {
    id: u32,
    is_plugin: bool,
    tab_id: u32,
}

/// Renomme l'onglet qui contient le pane `pane_id`. Par son id et non par `rename-tab`, qui vise
/// l'onglet qui a le focus.
pub fn rename_tab_of(pane_id: &str, name: &str) -> Result<(), String> {
    let out = Command::new("zellij")
        .args(["action", "list-panes", "--tab", "--json"])
        .output()
        .map_err(|err| format!("zellij : {err}"))?;
    let panes: Vec<Pane> = serde_json::from_slice(&out.stdout).map_err(|err| format!("zellij list-panes : {err}"))?;
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

fn check<'a>(own: Option<&str>, target: Option<&'a Zellij>) -> Result<&'a Zellij, String> {
    let target = target.ok_or("Instance hors zellij")?;
    if own != Some(target.session.as_str()) {
        return Err(format!("Instance dans la session zellij “{}”", target.session));
    }
    Ok(target)
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
