//! Actions rex (SPEC.md § rex) par sa CLI : saut vers le block d'une instance, fermeture, reprise
//! d'une session dans une nouvelle fenêtre, sessions headless en blocks détachés, copie de l'écran
//! et nom de la fenêtre d'xst.

use std::collections::HashSet;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::record::Rex;

/// Type des blocks terminal.
const TERMINAL: &str = "com.superlogical.terminal.shell";

/// Sans serveur lancé, la CLI en démarrerait un : xst n'agit que sur un rex déjà ouvert.
fn command(server: Option<&str>) -> Command {
    let mut command = Command::new("rex");
    command.arg("--autostart=false");
    if let Some(server) = server {
        command.args(["-S", server]);
    }
    command.stdin(Stdio::null());
    command
}

/// Sortie de la commande ; sinon la première ligne de son erreur.
fn output(command: &mut Command) -> Result<Vec<u8>, String> {
    let out = command.output().map_err(|err| match err.kind() {
        std::io::ErrorKind::NotFound => "rex introuvable".to_string(),
        _ => format!("rex : {err}"),
    })?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let reason = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("échec");
    let reason = reason.strip_prefix("Error: ").unwrap_or(reason);
    Err(format!("rex : {}", reason.chars().take(80).collect::<String>()))
}

/// Appelle la méthode `method` de l'API du serveur.
fn api(server: Option<&str>, method: &str, args: &Value) -> Result<Value, String> {
    let out = output(command(server).args(["api", "call", method, &args.to_string()]))?;
    serde_json::from_slice(&out).map_err(|err| format!("rex {method} : {err}"))
}

/// Focalise le block `target` et fait afficher sa fenêtre par les apps.
pub fn focus(target: &Rex) -> Result<(), String> {
    let server = target.server.as_deref();
    api(server, "session.focus_block", &json!({ "session_id": target.session, "block_id": target.block }))?;
    let window = window_of(server, &target.session, &target.block)?;
    show_window(server, &target.session, &window)
}

/// Fait afficher la session `session` et sa fenêtre `window` à chaque app connectée : une app
/// choisit seule la fenêtre qu'elle montre, sans suivre la fenêtre active du serveur.
fn show_window(server: Option<&str>, session: &str, window: &str) -> Result<(), String> {
    let args = json!({ "session_id": session, "window_id": window }).to_string();
    for app in apps(server)? {
        output(command(server).args(["-C", &app, "do", "session.select", "--args", &args])).map_err(|err| {
            if err.contains("Remote Control") {
                "Rex.app ne change de session qu'avec Remote Control (réglages, Rex Server)".to_string()
            } else {
                err
            }
        })?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct Clients {
    clients: Vec<Client>,
}

#[derive(Deserialize)]
struct Client {
    client_id: String,
    info: Option<ClientInfo>,
    #[serde(default)]
    connection_state: String,
}

#[derive(Deserialize)]
struct ClientInfo {
    kind: Option<String>,
}

/// Ids des apps rex connectées ; une app perdue reste listée un temps après sa reconnexion.
fn apps(server: Option<&str>) -> Result<Vec<String>, String> {
    let out = output(command(server).args(["client", "ls", "--json"]))?;
    let clients: Clients = serde_json::from_slice(&out).map_err(|err| format!("rex client ls : {err}"))?;
    Ok(clients
        .clients
        .into_iter()
        .filter(|c| c.info.as_ref().and_then(|i| i.kind.as_deref()) == Some("app") && c.connection_state == "connected")
        .map(|c| c.client_id)
        .collect())
}

/// Ferme le block `target` : rex envoie `SIGHUP` à omp, qui fait sa fermeture normale.
pub fn close(target: &Rex) -> Result<(), String> {
    close_block(target.server.as_deref(), &target.session, &target.block)
}

fn close_block(server: Option<&str>, session: &str, block: &str) -> Result<(), String> {
    output(command(server).args(["block", "close", "-s", session, block])).map(drop)
}

/// Ouvre `script` (fish) dans `cwd`, dans une nouvelle fenêtre de la session de `own`, affichée par
/// les apps.
pub fn resume(own: &Rex, script: &str, cwd: &Path) -> Result<(), String> {
    let server = own.server.as_deref();
    let block = json!({ "flavor": TERMINAL, "options": terminal(script, cwd) });
    let created = api(server, "session.new_window", &json!({ "session_id": own.session, "layout": { "block": block } }))?;
    let window = created["window_id"].as_str().ok_or("rex : fenêtre créée sans id")?;
    show_window(server, &own.session, window)
}

/// Lance `script` (fish) dans `cwd`, dans un block détaché de la session de `own`, de taille
/// `columns`×`rows` : sans app qui l'affiche, un block garde la taille de sa création.
pub fn spawn(own: &Rex, script: &str, cwd: &Path, (columns, rows): (u16, u16)) -> Result<(), String> {
    let mut options = terminal(script, cwd);
    options["size"] = json!({ "columns": columns, "rows": rows });
    let args = json!({ "session_id": own.session, "flavor": TERMINAL, "label": "omp", "options": options });
    api(own.server.as_deref(), "session.new_block", &args).map(drop)
}

/// Options d'un terminal qui lance `fish -C <script>` dans `cwd`, sans passer par un autre shell.
/// Le shell de connexion par lequel rex lancerait la commande n'a pas fish dans son `PATH` : fish
/// est pris dans celui d'xst, et sa configuration interactive rétablit le reste (omp).
fn terminal(script: &str, cwd: &Path) -> Value {
    let fish = std::env::var_os("PATH")
        .and_then(|path| std::env::split_paths(&path).map(|dir| dir.join("fish")).find(|p| p.is_file()))
        .map_or_else(|| "fish".to_string(), |p| p.to_string_lossy().into_owned());
    json!({ "shell": "none", "command": [fish, "-C", script], "cwd": cwd.to_string_lossy() })
}

/// Sort le block `target` de sa fenêtre ; il continue de tourner.
pub fn hide(target: &Rex) -> Result<(), String> {
    let args = json!({ "session_id": target.session, "block_id": target.block });
    api(target.server.as_deref(), "session.detach_block", &args).map(drop)
}

/// Place le block détaché `target` dans une nouvelle fenêtre, qui prend le focus. rex ne crée une
/// fenêtre qu'avec de nouveaux blocks : un block provisoire l'ouvre, `target` s'y place à côté,
/// puis le provisoire se ferme.
pub fn show(target: &Rex) -> Result<(), String> {
    let server = target.server.as_deref();
    let placeholder = json!({ "flavor": TERMINAL, "options": { "shell": "none", "command": ["/bin/sleep", "30"] } });
    let window = api(server, "session.new_window", &json!({ "session_id": target.session, "layout": { "block": placeholder } }))?;
    let placeholder = window["block_ids"][0].as_str().ok_or("rex : fenêtre créée sans block")?.to_string();
    let moved = api(
        server,
        "session.move_block",
        &json!({ "session_id": target.session, "block_id": target.block, "anchor_block_id": placeholder }),
    );
    let closed = close_block(server, &target.session, &placeholder);
    moved?;
    closed?;
    focus(target)
}

#[derive(Deserialize)]
struct Blocks {
    blocks: Vec<Block>,
}

#[derive(Deserialize)]
struct Block {
    block_id: String,
    #[serde(default)]
    placement: String,
    window_id: Option<String>,
}

fn blocks(server: Option<&str>, session: &str) -> Result<Vec<Block>, String> {
    let blocks = api(server, "session.list_blocks", &json!({ "session_id": session }))?;
    serde_json::from_value::<Blocks>(blocks).map(|b| b.blocks).map_err(|err| format!("rex list_blocks : {err}"))
}

/// Ids des blocks détachés des sessions `sessions` (serveur, session). Une session disparue ou
/// injoignable ne compte pas : ses blocks sont fermés avec elle.
pub fn detached(sessions: &[(Option<String>, String)]) -> HashSet<String> {
    sessions
        .iter()
        .filter_map(|(server, session)| blocks(server.as_deref(), session).ok())
        .flatten()
        .filter(|b| b.placement == "detached")
        .map(|b| b.block_id)
        .collect()
}

/// Écran visible du block `target`, en séquences VT.
pub fn screen(target: &Rex) -> Result<Vec<u8>, String> {
    #[derive(Deserialize)]
    struct Size {
        rows: usize,
    }
    let server = target.server.as_deref();
    let block = ["-s", target.session.as_str(), "-b", target.block.as_str()];
    let size = output(command(server).args(["block", "call"]).args(block).arg("size"))?;
    let Size { rows } = serde_json::from_slice(&size).map_err(|err| format!("rex size : {err}"))?;
    let capture = output(command(server).arg("capture").args(block).arg("--format=vt"))?;
    Ok(last_lines(&capture, rows))
}

/// Les `n` dernières lignes de `capture`, qui donne l'historique puis l'écran. Chaque ligne y
/// reprend son style depuis un `\e[0m` : une ligne coupée de celles d'avant garde ses couleurs.
fn last_lines(capture: &[u8], n: usize) -> Vec<u8> {
    let capture = capture.strip_suffix(b"\n").unwrap_or(capture);
    let mut lines: Vec<&[u8]> = capture.rsplit(|&b| b == b'\n').take(n).collect();
    lines.reverse();
    let lines: Vec<&[u8]> = lines.into_iter().map(|l| l.strip_suffix(b"\r").unwrap_or(l)).collect();
    lines.join(&b'\n')
}

/// Donne le nom `name` à la fenêtre du block `own`.
pub fn rename_window_of(own: &Rex, name: &str) -> Result<(), String> {
    let server = own.server.as_deref();
    let window = window_of(server, &own.session, &own.block)?;
    let args = json!({ "session_id": own.session, "window_id": window, "label": name });
    api(server, "session.set_window_label", &args).map(drop)
}

/// Fenêtre du block `block`.
fn window_of(server: Option<&str>, session: &str, block: &str) -> Result<String, String> {
    blocks(server, session)?
        .into_iter()
        .find(|b| b.block_id == block)
        .and_then(|b| b.window_id)
        .ok_or_else(|| format!("rex : block {block} hors fenêtre"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_lines_keeps_the_screen_under_the_history() {
        let capture = b"old\r\nscreen 1\r\n\x1b[0m\x1b[31mscreen 2\x1b[0m\n";
        assert_eq!(last_lines(capture, 2), b"screen 1\n\x1b[0m\x1b[31mscreen 2\x1b[0m");
        assert_eq!(last_lines(capture, 5), b"old\nscreen 1\n\x1b[0m\x1b[31mscreen 2\x1b[0m");
    }
}
