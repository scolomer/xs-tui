//! Plugin zellij d'xst : panes headless (SPEC.md § Sessions headless).
//!
//! xst l'appelle par `zellij pipe --plugin file:<wasm> --name xs-tui -- <commande>` ; la commande
//! tient sur des lignes : son nom, puis ses arguments. Le plugin répond `ok` ou un message
//! d'erreur sur la sortie du pipe. Un pane headless est un pane « supprimé » de zellij : affiché
//! dans aucun onglet, il reste rattaché à l'onglet de plus petit id et passe dans un autre quand
//! cet onglet se ferme.

use std::collections::BTreeMap;
use std::path::PathBuf;

use zellij_tile::prelude::*;

/// Nom des messages de pipe traités.
const PIPE_NAME: &str = "xs-tui";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    /// Lance `program args…` dans un pane headless, ouvert dans `cwd`.
    Spawn { cwd: PathBuf, program: PathBuf, args: Vec<String> },
    /// Sort le pane dans un nouvel onglet, qui prend le focus.
    Show(u32),
    /// Rend le pane headless.
    Hide(u32),
}

impl Command {
    fn parse(payload: &str) -> Result<Command, String> {
        let mut lines = payload.lines();
        let pane = |line: Option<&str>| {
            line.and_then(|l| l.trim().parse().ok()).ok_or_else(|| format!("id de pane attendu : {payload:?}"))
        };
        match lines.next() {
            Some("spawn") => {
                let (Some(cwd), Some(program)) = (lines.next(), lines.next()) else {
                    return Err("spawn : dossier et programme attendus".into());
                };
                let args = lines.map(str::to_owned).collect();
                Ok(Command::Spawn { cwd: cwd.into(), program: program.into(), args })
            }
            Some("show") => pane(lines.next()).map(Command::Show),
            Some("hide") => pane(lines.next()).map(Command::Hide),
            other => Err(format!("commande inconnue : {other:?}")),
        }
    }
}

#[derive(Default)]
struct State {
    granted: bool,
    /// Pipes reçus avant l'accord des permissions : (id du pipe CLI, contenu).
    pending: Vec<(String, String)>,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::RunCommands,
            PermissionType::ReadCliPipes,
        ]);
        subscribe(&[EventType::PermissionRequestResult]);
    }

    fn update(&mut self, event: Event) -> bool {
        if let Event::PermissionRequestResult(PermissionStatus::Granted) = event {
            self.granted = true;
            // Affiché seulement pour la demande de permissions, au premier lancement.
            hide_self();
            for (cli, payload) in std::mem::take(&mut self.pending) {
                answer(&cli, &payload);
            }
        }
        false
    }

    fn pipe(&mut self, message: PipeMessage) -> bool {
        let PipeSource::Cli(cli) = &message.source else { return false };
        // Chaque `zellij pipe` arrive aussi une seconde fois sans contenu.
        let Some(payload) = message.payload.filter(|p| !p.trim().is_empty()) else { return false };
        if message.name != PIPE_NAME {
            return false;
        }
        if self.granted {
            answer(cli, &payload);
        } else {
            // Le pipe CLI reste bloqué jusqu'à la réponse.
            self.pending.push((cli.clone(), payload));
        }
        false
    }

    fn render(&mut self, _rows: usize, _cols: usize) {
        println!("xs-tui : accorder les permissions pour les sessions headless d'xst.");
    }
}

fn answer(cli: &str, payload: &str) {
    let reply = match Command::parse(payload) {
        Ok(command) => {
            run(command);
            "ok".to_string()
        }
        Err(message) => message,
    };
    cli_pipe_output(cli, &format!("{reply}\n"));
    unblock_cli_pipe_input(cli);
}

fn run(command: Command) {
    match command {
        Command::Spawn { cwd, program, args } => {
            let command = CommandToRun { path: program, args, cwd: Some(cwd) };
            if let Some(pane) = open_command_pane_background(command, BTreeMap::new()) {
                // Un pane ouvert en arrière-plan a une taille par défaut (92×28) ; passé par un
                // onglet, il garde la taille de la session une fois caché. L'onglet, resté seul
                // vide, se ferme.
                break_panes_to_new_tab(&[pane], None, false);
                hide_pane_with_id(pane);
            }
        }
        Command::Show(pane) => {
            break_panes_to_new_tab(&[PaneId::Terminal(pane)], None, true);
        }
        // Seul dans son onglet, le pane caché quitte l'onglet, qui se ferme.
        Command::Hide(pane) => hide_pane_with_id(PaneId::Terminal(pane)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spawn_with_arguments_and_rejects_missing_ids() {
        assert_eq!(
            Command::parse("spawn\n/tmp\nfish\n-C\ncd /tmp"),
            Ok(Command::Spawn { cwd: "/tmp".into(), program: "fish".into(), args: vec!["-C".into(), "cd /tmp".into()] })
        );
        assert_eq!(Command::parse("show\n42"), Ok(Command::Show(42)));
        assert!(Command::parse("hide\n").is_err());
        assert!(Command::parse("spawn\n/tmp").is_err());
    }
}
