//! Connexion au relai (SPEC xs-parley §6.3) : une WebSocket par endpoint, dans son thread
//! `xsp-<endpoint>`, reconnectée avec un délai exponentiel. Le relai est aveugle : `env` et
//! `cert_box` sont opaques ici.

use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant};

use serde_json::Value;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use super::codec::random_bytes;

const MAX_FRAME_BYTES: usize = 64 * 1024;
const READ_TIMEOUT: Duration = Duration::from_millis(500);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// Une connexion restée ouverte aussi longtemps remet le délai de reconnexion à son minimum.
const STABLE: Duration = Duration::from_secs(10);

#[derive(Debug)]
pub enum ConnEvent {
    Up(String),
    Down(String, String),
    /// `(endpoint, parley_id, env)`.
    Deliver(String, String, Value),
    /// Une autre connexion a repris notre empreinte dans cette parley.
    Replaced(String, String),
    Problem(String, String),
}

/// Côté manager d'une connexion : les trames à envoyer. Libérer la dernière copie arrête le
/// thread.
#[derive(Clone)]
pub struct Connection {
    out: Sender<String>,
}

static FRAME_ID: AtomicU64 = AtomicU64::new(0);

impl Connection {
    /// Ouvre la connexion dans son thread ; ses événements arrivent sur `events`, convertis en `E`.
    pub fn spawn<E: From<ConnEvent> + Send + 'static>(endpoint: String, events: Sender<E>) -> Connection {
        let (out, rx) = mpsc::channel();
        let name = format!("xsp-{endpoint}");
        std::thread::Builder::new().name(name).spawn(move || run(&endpoint, &events, &rx)).expect("thread du relai");
        Connection { out }
    }

    /// Connexion sans thread, dont les trames arrivent sur le récepteur rendu.
    #[cfg(test)]
    pub fn detached() -> (Connection, Receiver<String>) {
        let (out, rx) = mpsc::channel();
        (Connection { out }, rx)
    }

    /// Ajoute `"v": 1` et un `"id"` à `frame` et la confie au thread ; perdue si la connexion est
    /// tombée (elle sera rejointe à la reconnexion).
    pub fn send_frame(&self, mut frame: Value) {
        frame["v"] = 1.into();
        frame["id"] = format!("c{}", FRAME_ID.fetch_add(1, Ordering::Relaxed) + 1).into();
        let _ = self.out.send(frame.to_string());
    }
}

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

fn run<E: From<ConnEvent>>(endpoint: &str, events: &Sender<E>, out: &Receiver<String>) {
    // Faux quand le manager a disparu : le thread s'arrête.
    let emit = |event: ConnEvent| events.send(event.into()).is_ok();
    let mut attempt = 0u32;
    loop {
        match tungstenite::connect(endpoint) {
            Ok((mut ws, _)) => {
                let opened = Instant::now();
                set_read_timeout(&mut ws);
                if !emit(ConnEvent::Up(endpoint.into())) {
                    return;
                }
                // Trames confiées avant la connexion : sans objet, le manager rejoint à `Up`.
                while out.try_recv().is_ok() {}
                let reason = match session(endpoint, &mut ws, &emit, out) {
                    None => {
                        let _ = ws.close(None);
                        let _ = ws.flush();
                        return;
                    }
                    Some(reason) => reason,
                };
                if opened.elapsed() >= STABLE {
                    attempt = 0;
                }
                if !emit(ConnEvent::Down(endpoint.into(), reason)) {
                    return;
                }
            }
            Err(e) => {
                let hint = if endpoint.starts_with("wss:") && e.to_string().contains("TLS") { " (relai sans TLS ?)" } else { "" };
                if !emit(ConnEvent::Problem(endpoint.into(), format!("connexion impossible : {e}{hint}"))) {
                    return;
                }
            }
        }
        // Délai 1 s × 2^n plafonné à 60 s, plus 0 à 25 % ; la libération de la connexion l'interrompt.
        let base = BACKOFF_MIN.saturating_mul(1 << attempt.min(6)).min(BACKOFF_MAX);
        attempt = attempt.saturating_add(1);
        let jitter = f64::from(random_bytes::<1>()[0]) / 255.0 * 0.25;
        let deadline = Instant::now() + base.mul_f64(1.0 + jitter).min(BACKOFF_MAX);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            match out.recv_timeout(left) {
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

fn set_read_timeout(ws: &mut Socket) {
    let _ = match ws.get_mut() {
        MaybeTlsStream::Plain(s) => s.set_read_timeout(Some(READ_TIMEOUT)),
        MaybeTlsStream::Rustls(s) => s.sock.set_read_timeout(Some(READ_TIMEOUT)),
        _ => Ok(()),
    };
}

/// Une connexion ouverte : lit les trames du relai et envoie celles du manager. `None` quand la
/// connexion est libérée, sinon la raison de la coupure.
fn session(endpoint: &str, ws: &mut Socket, emit: &dyn Fn(ConnEvent) -> bool, out: &Receiver<String>) -> Option<String> {
    loop {
        loop {
            match out.try_recv() {
                Ok(frame) if frame.len() > MAX_FRAME_BYTES => {
                    emit(ConnEvent::Problem(endpoint.into(), "too_large".into()));
                }
                Ok(frame) => {
                    if let Err(e) = ws.send(Message::text(frame)) {
                        return Some(e.to_string());
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return None,
            }
        }
        match ws.read() {
            Ok(Message::Text(text)) => {
                if let Some(event) = parse_frame(endpoint, text.as_str()) {
                    emit(event);
                }
            }
            // tungstenite met le Pong en file : le prochain `flush` l'envoie.
            Ok(Message::Ping(_)) => {
                if let Err(e) = ws.flush() {
                    return Some(e.to_string());
                }
            }
            Ok(Message::Close(frame)) => return Some(frame.map_or_else(|| "fermée par le relai".into(), |f| f.reason.to_string())),
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                let _ = ws.flush();
            }
            Err(e) => return Some(e.to_string()),
        }
    }
}

/// Événement d'une trame du relai ; `None` pour une trame ignorée (réponse `ok`/`sent`, présence,
/// erreur `offline` d'une réponse au mieux à un master parti).
fn parse_frame(endpoint: &str, text: &str) -> Option<ConnEvent> {
    let frame: Value = serde_json::from_str(text).ok()?;
    let str_field = |k: &str| frame.get(k).and_then(Value::as_str);
    match str_field("t")? {
        "deliver" => Some(ConnEvent::Deliver(endpoint.into(), str_field("parley_id")?.into(), frame.get("env")?.clone())),
        "error" => {
            let code = str_field("code").unwrap_or("error");
            let message = str_field("message").unwrap_or(code);
            match (code, str_field("parley_id"), str_field("id")) {
                ("replaced", Some(parley_id), _) => Some(ConnEvent::Replaced(endpoint.into(), parley_id.into())),
                ("offline", _, Some(_)) => None,
                _ => Some(ConnEvent::Problem(endpoint.into(), format!("{code} — {message}"))),
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_frames_map_to_events() {
        let e = "wss://r/ws";
        assert!(matches!(parse_frame(e, r#"{"v":1,"t":"deliver","parley_id":"p","env":{"h":{}}}"#), Some(ConnEvent::Deliver(_, p, _)) if p == "p"));
        assert!(matches!(parse_frame(e, r#"{"v":1,"t":"error","code":"replaced","parley_id":"p"}"#), Some(ConnEvent::Replaced(_, p)) if p == "p"));
        assert!(parse_frame(e, r#"{"v":1,"t":"error","id":"c3","code":"offline"}"#).is_none());
        assert!(matches!(parse_frame(e, r#"{"v":1,"t":"error","id":"c3","code":"not_joined","message":"x"}"#), Some(ConnEvent::Problem(_, m)) if m == "not_joined — x"));
        assert!(parse_frame(e, r#"{"v":1,"t":"sent","id":"c4"}"#).is_none());
        assert!(parse_frame(e, r#"{"v":1,"t":"presence","parley_id":"p","fp":"f","online":true}"#).is_none());
    }
}
