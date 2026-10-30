//! Saut vers le pane zellij d'une instance (`zellij action focus-pane-id`).

use std::process::Command;

use crate::record::Zellij;

/// Vrai si `focus` peut atteindre `target` depuis la session zellij `own`.
pub fn reachable(own: Option<&str>, target: Option<&Zellij>) -> bool {
    check(own, target).is_ok()
}

/// Focalise le pane `target` ; message d'erreur à afficher dans la barre sinon.
pub fn focus(own: Option<&str>, target: Option<&Zellij>) -> Result<(), String> {
    let target = check(own, target)?;
    let out = Command::new("zellij")
        .args(["action", "focus-pane-id", &format!("terminal_{}", target.pane_id)])
        .output()
        .map_err(|err| format!("zellij : {err}"))?;
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
