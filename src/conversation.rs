//! Conversation d'un thread Capy dans la copie `d` (SPEC.md § Threads Capy) : derniers messages lus
//! par `GET /threads/{id}/messages`, puis suivi en direct par le flux Server-Sent Events du thread,
//! ouvert seulement tant que la copie montre ce thread.

use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use crate::capy::Api;
use crate::mirror::Update;
use crate::watch::Event;

/// Entrées gardées : la copie n'en montre que la fin.
const KEEP: usize = 200;
/// Taille demandée pour la dernière page de la conversation.
const PAGE: usize = 50;
/// Plus grand id d'événement possible (ULID) : `before` donne alors la page la plus récente.
const LATEST: &str = "7ZZZZZZZZZZZZZZZZZZZZZZZZZ";
/// Attente avant de rouvrir un flux coupé, celle qu'annonce Capy (`retry: 2000`).
const RETRY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    User,
    Assistant,
    Tool,
}

/// Une entrée de la conversation : message de l'utilisateur, réponse de l'agent ou résumé d'outil.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub source: Source,
    /// Outil d'une entrée `tool` (`execute`, `wait`…).
    pub tool: Option<String>,
    pub text: String,
}

/// Ce que montre la copie : la conversation enregistrée, puis le tour en cours.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conversation {
    pub entries: Vec<Entry>,
    /// Réponse de l'agent en cours d'écriture, par `callId`.
    pub live: Vec<(String, String)>,
    /// Outil en cours d'appel : `(callId, nom, résumé)`.
    pub tool_call: Option<(String, String, String)>,
    /// Le flux est fini : thread archivé ou supprimé.
    pub ended: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Message {
    id: String,
    source: Source,
    tool: Option<String>,
    #[serde(default)]
    text: String,
    #[serde(default)]
    calls: Vec<Call>,
    call_id: Option<String>,
}

#[derive(Deserialize)]
struct Call {
    tool: String,
}

#[derive(Deserialize)]
struct TranscriptPage {
    items: Vec<Message>,
}

impl Message {
    /// Entrée affichée ; un outil sans texte montre les commandes qu'il a lancées.
    fn entry(self) -> Entry {
        let text = if self.text.is_empty() && !self.calls.is_empty() {
            self.calls.iter().map(|c| c.tool.as_str()).collect::<Vec<_>>().join(", ")
        } else {
            self.text
        };
        Entry { id: self.id, source: self.source, tool: self.tool, text }
    }
}

/// Id d'événement (ULID, 26 caractères) : seuls ceux-là servent à reprendre un flux. Les entrées
/// d'outil ont l'id de l'appel (`toolu_…`).
fn is_event_id(id: &str) -> bool {
    id.len() == 26 && id.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
}

impl Conversation {
    /// Ajoute une entrée enregistrée, ou remplace celle de même id (un flux repris rejoue les entrées
    /// qui suivent son point de reprise).
    fn record(&mut self, message: Message) {
        // Une réponse enregistrée remplace ses fragments, un outil enregistré son appel en cours.
        if let Some(call) = &message.call_id {
            self.live.retain(|(id, _)| id != call);
        }
        if self.tool_call.as_ref().is_some_and(|(call, _, _)| *call == message.id) {
            self.tool_call = None;
        }
        let entry = message.entry();
        match self.entries.iter_mut().find(|e| e.id == entry.id) {
            Some(known) => *known = entry,
            None => self.entries.push(entry),
        }
        let excess = self.entries.len().saturating_sub(KEEP);
        self.entries.drain(..excess);
    }

    /// Applique une trame du flux ; vrai si l'affichage change.
    fn apply(&mut self, event: &str, data: &Value) -> bool {
        let text = |k: &str| data.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        match event {
            "transcript" => match serde_json::from_value::<Message>(data.clone()) {
                Ok(message) => self.record(message),
                Err(_) => return false,
            },
            "assistant" => {
                let call = text("callId");
                match self.live.iter_mut().find(|(id, _)| *id == call) {
                    Some((_, live)) => live.push_str(&text("text")),
                    None => self.live.push((call, text("text"))),
                }
            }
            "tool_call" => self.tool_call = Some((text("callId"), text("name"), text("text"))),
            // Les fragments jamais enregistrés appartiennent à un tour interrompu.
            "run" if data.get("phase").and_then(Value::as_str) == Some("end") => {
                if self.live.is_empty() && self.tool_call.is_none() {
                    return false;
                }
                self.live.clear();
                self.tool_call = None;
            }
            "done" => self.ended = true,
            _ => return false,
        }
        true
    }
}

/// Trame Server-Sent Events : `event`, `data` (une ligne JSON) et `id` éventuel.
#[derive(Default)]
struct Frame {
    event: String,
    data: String,
    id: Option<String>,
}

impl Frame {
    /// Ajoute une ligne ; renvoie la trame complète à la ligne vide qui la termine.
    fn feed(&mut self, line: &str) -> Option<Frame> {
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return (!self.event.is_empty() || !self.data.is_empty()).then(|| std::mem::take(self));
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => self.event = value.to_string(),
            "data" => {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value);
            }
            "id" => self.id = Some(value.to_string()),
            _ => {}
        }
        None
    }
}

/// Suit le thread `thread` jusqu'à `stop` : derniers messages, puis le flux, rouvert après une
/// coupure. Chaque changement part en `Update::Conversation` ; une erreur définitive en `Failed`.
pub fn follow(api: &Api, thread: &str, stop: &AtomicBool, generation: u64, tx: &Sender<Event>) {
    let send = |update: Update| tx.send(Event::Pane(generation, update)).is_ok();
    let mut conversation = Conversation::default();
    let page: TranscriptPage = match api.get(&format!("/threads/{thread}/messages?before={LATEST}&limit={PAGE}")) {
        Ok(page) => page,
        Err(message) => {
            send(Update::Failed(format!("Conversation illisible : {message}")));
            return;
        }
    };
    let mut cursor = page.items.iter().rev().map(|m| m.id.as_str()).find(|id| is_event_id(id)).map(str::to_string);
    for message in page.items {
        conversation.record(message);
    }
    if stop.load(Ordering::Relaxed) || !send(Update::Conversation(conversation.clone())) {
        return;
    }
    while !stop.load(Ordering::Relaxed) {
        let reader = match api.stream(&format!("/threads/{thread}/stream"), cursor.as_deref()) {
            Ok(reader) => BufReader::new(reader),
            Err(message) if message.contains("refusée") || message.contains("introuvable") => {
                send(Update::Failed(format!("Flux interrompu : {message}")));
                return;
            }
            Err(_) => {
                std::thread::sleep(RETRY);
                continue;
            }
        };
        let mut frame = Frame::default();
        // La lecture bloque jusqu'à la trame suivante : au plus 15 s, avec le battement de Capy.
        for line in reader.lines() {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let Ok(line) = line else { break };
            let Some(complete) = frame.feed(&line) else { continue };
            if let Some(id) = complete.id {
                cursor = Some(id);
            }
            let data: Value = serde_json::from_str(&complete.data).unwrap_or(Value::Null);
            if complete.event == "error" {
                let message = data.get("message").and_then(Value::as_str).unwrap_or("erreur de Capy");
                send(Update::Failed(format!("Flux interrompu : {message}")));
                return;
            }
            if conversation.apply(&complete.event, &data) && !send(Update::Conversation(conversation.clone())) {
                return;
            }
            if conversation.ended {
                return;
            }
        }
        // Coupure sans trame de fin : reprise après le dernier événement lu.
        std::thread::sleep(RETRY);
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn frames(text: &str) -> Vec<Frame> {
        let mut frame = Frame::default();
        text.lines().filter_map(|l| frame.feed(l)).collect()
    }

    #[test]
    fn sse_lines_make_frames_with_their_event_id() {
        let got = frames("event: status\ndata: {\"status\":\"working\"}\n\nretry: 2000\n\nid: 01J\nevent: transcript\ndata: {}\n\n");
        let got: Vec<_> = got.iter().map(|f| (f.event.as_str(), f.data.as_str(), f.id.as_deref())).collect();
        assert_eq!(got, [("status", "{\"status\":\"working\"}", None), ("transcript", "{}", Some("01J"))]);
    }

    #[test]
    fn live_frames_are_replaced_by_the_recorded_entries() {
        let mut c = Conversation::default();
        assert!(c.apply("assistant", &json!({"callId": "c1", "text": "Tests "})));
        assert!(c.apply("assistant", &json!({"callId": "c1", "text": "pass."})));
        assert!(c.apply("tool_call", &json!({"callId": "toolu_1", "name": "bash", "text": "cargo test"})));
        assert_eq!(c.live, [("c1".to_string(), "Tests pass.".to_string())]);

        let tool = json!({"id": "toolu_1", "source": "tool", "tool": "execute", "text": "", "calls": [{"tool": "bash", "durationMs": 3}], "createdAt": "t"});
        assert!(c.apply("transcript", &tool));
        assert_eq!(c.tool_call, None);
        assert_eq!(c.entries[0].text, "bash");
        let reply = json!({"id": "01J", "source": "assistant", "callId": "c1", "text": "Tests pass.", "createdAt": "t"});
        assert!(c.apply("transcript", &reply));
        assert!(c.live.is_empty());
        // Un flux repris rejoue l'entrée : elle n'est pas dupliquée.
        assert!(c.apply("transcript", &reply));
        assert_eq!(c.entries.len(), 2);
        assert!(!c.apply("heartbeat", &json!({})));
    }

    #[test]
    fn only_ulids_resume_a_stream() {
        assert!(is_event_id("01M7SDENR3HC6PKABAX469SY29"));
        assert!(!is_event_id("toolu_01PEP6qEFbvJzpRrYFiq8dv2"));
    }
}
