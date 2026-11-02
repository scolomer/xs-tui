//! Consommation des abonnements Anthropic et OpenAI (SPEC.md § Consommation), lue avec
//! `omp usage --json` : omp garde ces rapports en cache et les tient à jour avec les en-têtes de
//! réponse des modèles, agz n'a donc ni identifiants ni appel réseau à faire.
//!
//! Un thread relit la consommation au démarrage, après une fin de tour (au plus une fois par
//! `MIN_GAP`), juste après le prochain reset d'une fenêtre, et toutes les `PERIOD` pour capter la
//! consommation faite hors omp.

use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;

use crate::watch::Event;

const PERIOD: Duration = Duration::from_secs(15 * 60);
const MIN_GAP: Duration = Duration::from_secs(60);
/// Délai après un reset avant la relecture, le temps que le fournisseur ouvre la nouvelle fenêtre.
const RESET_MARGIN: Duration = Duration::from_secs(5);

/// Fournisseurs affichés : (id dans omp, nom affiché).
const PROVIDERS: [(&str, &str); 2] = [("anthropic", "Anthropic"), ("openai-codex", "OpenAI")];

/// Jauges d'un compte.
#[derive(Clone, Debug, PartialEq)]
pub struct Account {
    /// Nom du fournisseur, suivi de l'email du compte s'il y en a plusieurs pour ce fournisseur.
    pub name: String,
    pub gauges: Vec<Gauge>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Gauge {
    /// Fenêtre (`5 h`, `7 j`) et compteur secondaire éventuel (`7 j Fable`).
    pub label: String,
    /// Part consommée, entre 0 et 1.
    pub used: f64,
    /// Fin de la fenêtre, en millisecondes depuis l'epoch.
    pub resets_at: Option<u64>,
    pub status: Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Warning,
    Exhausted,
    #[serde(other)]
    Unknown,
}

/// Lance le thread de lecture ; chaque envoi sur le canal retourné signale une fin de tour.
pub fn spawn_refresher(tx: Sender<Event>) -> Sender<()> {
    let (turns, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("omp-usage".into())
        .spawn(move || refresh_loop(&rx, &tx))
        .expect("thread de consommation");
    turns
}

fn refresh_loop(turns: &Receiver<()>, tx: &Sender<Event>) {
    loop {
        let result = fetch();
        let fetched = Instant::now();
        let mut due = fetched + PERIOD;
        if let Some(reset) = result.as_ref().ok().and_then(|accounts| next_reset(accounts, now_ms())) {
            due = due.min(fetched + reset + RESET_MARGIN);
        }
        if tx.send(Event::Usage(result)).is_err() {
            return;
        }
        loop {
            match turns.recv_timeout(due.saturating_duration_since(Instant::now())) {
                Ok(()) => due = due.min(fetched + MIN_GAP),
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// Délai jusqu'au prochain reset à venir.
fn next_reset(accounts: &[Account], now_ms: u64) -> Option<Duration> {
    let gauges = accounts.iter().flat_map(|a| &a.gauges);
    let next = gauges.filter_map(|g| g.resets_at).filter(|&at| at > now_ms).min()?;
    Some(Duration::from_millis(next - now_ms))
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn fetch() -> Result<Vec<Account>, String> {
    let out = Command::new("omp")
        .args(["usage", "--json", "--no-extensions"])
        // Sinon chaque appel laisse un journal `~/.omp/logs/omp.<jour>.<pid>.log`, gardé 5 jours.
        .env("OMP_LOG_LEVEL", "error")
        .stdin(Stdio::null())
        .output()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => "omp introuvable".to_string(),
            _ => format!("omp usage : {err}"),
        })?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = stderr.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("échec");
        return Err(format!("omp usage : {reason}"));
    }
    parse(&out.stdout)
}

#[derive(Deserialize)]
struct Usage {
    reports: Vec<Report>,
}

#[derive(Deserialize)]
struct Report {
    provider: String,
    #[serde(default)]
    metadata: Metadata,
    #[serde(default)]
    limits: Vec<Limit>,
}

#[derive(Deserialize, Default)]
struct Metadata {
    email: Option<String>,
}

#[derive(Deserialize)]
struct Limit {
    scope: Scope,
    window: Option<Window>,
    amount: Amount,
    status: Option<Status>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Scope {
    window_id: Option<String>,
    tier: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Window {
    id: Option<String>,
    resets_at: Option<f64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Amount {
    used_fraction: Option<f64>,
}

fn parse(bytes: &[u8]) -> Result<Vec<Account>, String> {
    let usage: Usage = serde_json::from_slice(bytes).map_err(|_| "omp usage : réponse illisible".to_string())?;
    let mut accounts = Vec::new();
    for (id, name) in PROVIDERS {
        let reports: Vec<&Report> = usage.reports.iter().filter(|r| r.provider == id).collect();
        for report in &reports {
            let name = match (&report.metadata.email, reports.len()) {
                (Some(email), 2..) => format!("{name} · {email}"),
                _ => name.to_string(),
            };
            let gauges = report.limits.iter().filter_map(gauge).collect();
            accounts.push(Account { name, gauges });
        }
    }
    Ok(accounts)
}

fn gauge(limit: &Limit) -> Option<Gauge> {
    let window = limit.window.as_ref();
    let id = window.and_then(|w| w.id.as_deref()).or(limit.scope.window_id.as_deref())?;
    let mut label = window_label(id);
    if let Some(tier) = &limit.scope.tier {
        label = format!("{label} {}", capitalize(tier));
    }
    Some(Gauge {
        label,
        used: limit.amount.used_fraction?.clamp(0.0, 1.0),
        resets_at: window.and_then(|w| w.resets_at).map(|ms| ms as u64),
        status: limit.status.unwrap_or(Status::Unknown),
    })
}

/// `5h` → `5 h`, `7d` → `7 j` ; un autre id est gardé tel quel.
fn window_label(id: &str) -> String {
    let (count, unit) = id.split_at(id.len().saturating_sub(1));
    let unit = match unit {
        "h" => "h",
        "d" => "j",
        _ => return id.to_string(),
    };
    if count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
        return id.to_string();
    }
    format!("{count} {unit}")
}

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extrait de `omp usage --json` (champs inutilisés retirés).
    const SAMPLE: &str = r#"{"reports":[
      {"provider":"openai-codex","metadata":{"email":"a@b.c"},"limits":[
        {"scope":{"provider":"openai-codex","windowId":"5h"},"window":{"id":"5h","resetsAt":1793623169000},
         "amount":{"usedFraction":0,"unit":"percent"},"status":"ok"}]},
      {"provider":"mcp_oauth:profile:default:https://mcp.notion.com/mcp","limits":[]},
      {"provider":"anthropic","metadata":{"email":"a@b.c"},"limits":[
        {"scope":{"windowId":"7d"},"window":{"id":"7d","resetsAt":1793750400394},"amount":{"usedFraction":0.22},"status":"warning"},
        {"scope":{"windowId":"7d","tier":"fable"},"window":{"id":"7d","resetsAt":1793750400394},"amount":{"usedFraction":0.04},"status":"ok"}]}
    ]}"#;

    #[test]
    fn keeps_anthropic_then_openai_with_tier_labels() {
        let accounts = parse(SAMPLE.as_bytes()).unwrap();
        let names: Vec<&str> = accounts.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Anthropic", "OpenAI"]);
        let labels: Vec<&str> = accounts[0].gauges.iter().map(|g| g.label.as_str()).collect();
        assert_eq!(labels, ["7 j", "7 j Fable"]);
        assert_eq!(accounts[0].gauges[0].status, Status::Warning);
        assert_eq!(accounts[0].gauges[0].resets_at, Some(1_793_750_400_394));
        assert_eq!(accounts[1].gauges[0].label, "5 h");
    }

    #[test]
    fn next_reset_ignores_past_windows() {
        let accounts = parse(SAMPLE.as_bytes()).unwrap();
        assert_eq!(next_reset(&accounts, 1_793_700_000_000), Some(Duration::from_millis(50_400_394)));
        assert_eq!(next_reset(&accounts, 1_800_000_000_000), None);
    }
}
