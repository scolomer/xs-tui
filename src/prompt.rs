//! Envoi d'un prompt avec `w` (SPEC.md § Envoi de prompt) : saisie dans la barre du bas. Pour une
//! instance omp, dépôt dans `~/.omp/run/xs-tui/<pid>/inbox/`, que l'extension xs-tui de l'instance
//! surveille et transmet à omp comme un message tapé dans son éditeur ; pour un thread Capy, envoi
//! par l'API de Capy (`capy::Api::send_message`).

use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Boîte de dépôt, dans le dossier publié de l'instance ; l'extension la crée au démarrage.
const INBOX: &str = "inbox";
/// Départage deux envois de la même microseconde, résolution de l'horloge sur macOS.
static SENT: AtomicU64 = AtomicU64::new(0);

/// Destinataire d'un prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// Instance omp, par son pid.
    Omp(i32),
    /// Thread Capy, par son id.
    Capy(String),
}

/// Prompt en cours de saisie pour `target`.
pub struct Prompt {
    pub target: Target,
    /// Titre de la session ou du thread à l'ouverture de la saisie.
    pub title: String,
    pub text: String,
}

impl Prompt {
    pub fn new(target: Target, title: String) -> Prompt {
        Prompt { target, title, text: String::new() }
    }

    /// Ajoute un texte collé ; les fins de ligne `\r\n` et `\r` du terminal deviennent `\n`.
    pub fn paste(&mut self, text: &str) {
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\r' {
                chars.next_if_eq(&'\n');
                self.text.push('\n');
            } else {
                self.text.push(c);
            }
        }
    }
}

/// Dépose `text` dans la boîte de l'instance `pid` : écriture dans un fichier caché puis `rename`,
/// l'extension ne voit jamais un prompt partiel. Les noms suivent l'ordre d'envoi.
pub fn send(run_dir: &Path, pid: i32, text: &str) -> Result<(), String> {
    let inbox = run_dir.join(pid.to_string()).join(INBOX);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let name = format!("{nanos:020}-{:06}.txt", SENT.fetch_add(1, Ordering::Relaxed));
    let tmp = inbox.join(format!(".{name}.tmp"));
    if let Err(err) = std::fs::write(&tmp, text) {
        // L'extension crée la boîte au démarrage d'omp : une instance lancée avant sa mise à jour
        // n'en a pas.
        return Err(if err.kind() == ErrorKind::NotFound {
            "Extension xs-tui trop ancienne dans cette instance : relancer omp".to_string()
        } else {
            format!("Prompt non envoyé : {err}")
        });
    }
    std::fs::rename(&tmp, inbox.join(name)).map_err(|err| {
        let _ = std::fs::remove_file(&tmp);
        format!("Prompt non envoyé : {err}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_turns_terminal_line_endings_into_newlines() {
        let mut prompt = Prompt::new(Target::Omp(1), String::new());
        prompt.paste("a\r\nb\rc\nd");
        assert_eq!(prompt.text, "a\nb\nc\nd");
    }

    #[test]
    fn send_reports_an_instance_without_inbox_and_delivers_in_order() {
        let run_dir = std::env::temp_dir().join(format!("xst-prompt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&run_dir);
        std::fs::create_dir_all(run_dir.join("42")).unwrap();
        assert_eq!(send(&run_dir, 42, "x"), Err("Extension xs-tui trop ancienne dans cette instance : relancer omp".into()));

        let inbox = run_dir.join("42").join(INBOX);
        std::fs::create_dir(&inbox).unwrap();
        send(&run_dir, 42, "un").unwrap();
        send(&run_dir, 42, "deux").unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&inbox).unwrap().map(|e| e.unwrap().file_name()).collect();
        names.sort();
        let texts: Vec<_> = names.iter().map(|n| std::fs::read_to_string(inbox.join(n)).unwrap()).collect();
        std::fs::remove_dir_all(&run_dir).unwrap();
        assert_eq!(texts, ["un", "deux"]);
    }
}
