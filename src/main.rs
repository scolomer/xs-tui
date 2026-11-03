//! agz : tableau de bord des instances omp interactives et de leurs sous-agents.
//!
//! L'extension agentz (`extension/`) publie l'état de chaque agent dans `~/.omp/run/agentz/<pid>/` ;
//! ce binaire surveille ces fichiers, la vie des pids et les HEAD git, et redessine à chaque
//! changement (voir SPEC.md).

mod git;
mod mirror;
mod model;
mod pins;
mod proc;
mod record;
mod ui;
mod usage;
mod watch;
mod zellij;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use notify::RecommendedWatcher;
use ratatui::widgets::ListState;

use mirror::Mirror;
use model::{CopySource, Key, Model, ROOT_ID, Row, RowKind};
use record::{Kind, Parsed, State};
use watch::Event;

const SPINNER_FRAME: Duration = Duration::from_millis(100);
const MESSAGE_TTL: Duration = Duration::from_secs(3);

struct App {
    model: Model,
    rows: Vec<Row>,
    list: ListState,
    run_dir: PathBuf,
    /// `~/.omp` : baux de propriété des sessions dans `run/session-owners/`.
    omp_dir: PathBuf,
    /// `~/.omp/agent` : `session-pins.json` et fichiers de session.
    agent_dir: PathBuf,
    home: PathBuf,
    own_zellij: Option<String>,
    tx: Sender<Event>,
    watcher: RecommendedWatcher,
    /// Gitdirs déjà ajoutés au watcher.
    git_dirs: HashSet<PathBuf>,
    /// Demandes au thread de statut git : dossiers des instances et des sessions dormantes.
    git: Sender<Vec<PathBuf>>,
    /// Épingles non publiées dont `model.headers` a été lu : relu quand cet ensemble change.
    headers_for: Vec<String>,
    /// Copie de la session sélectionnée, ouverte par `d` (SPEC.md § Copie de session).
    mirror: Option<Mirror>,
    /// Signale une fin de tour au thread de consommation (SPEC.md § Consommation).
    usage: Sender<()>,
    /// Racine dont la fermeture attend un second `x`, et heure du premier.
    close_pending: Option<(Key, Instant)>,
}

fn main() {
    let Some(home) = std::env::var_os("HOME") else {
        eprintln!("agz: HOME non défini");
        std::process::exit(1);
    };
    let home = PathBuf::from(home);
    let omp_dir = home.join(".omp");
    let agent_dir = omp_dir.join("agent");
    let run_dir = omp_dir.join("run/agentz");
    let (tx, rx) = mpsc::channel();
    let watcher = match watch::start(&run_dir, &agent_dir, tx.clone()) {
        Ok(w) => w,
        Err(err) => {
            eprintln!("agz: surveillance de {} impossible : {err}", run_dir.display());
            std::process::exit(1);
        }
    };
    let mut app = App {
        model: Model::default(),
        rows: Vec::new(),
        list: ListState::default(),
        run_dir,
        omp_dir,
        agent_dir,
        home,
        own_zellij: std::env::var("ZELLIJ_SESSION_NAME").ok(),
        tx: tx.clone(),
        watcher,
        git_dirs: HashSet::new(),
        git: git::spawn_refresher(tx.clone()),
        headers_for: Vec::new(),
        mirror: None,
        usage: usage::spawn_refresher(tx.clone()),
        close_pending: None,
    };

    let mut terminal = ratatui::init();
    watch::spawn_terminal_reader(tx);
    app.rescan();
    app.load_pins();
    app.refresh_headers();
    app.request_git();
    app.rebuild();
    // Seulement au démarrage : la fonction fish de l'onglet remet son nom à la commande suivante.
    if app.own_zellij.is_some()
        && let Ok(pane) = std::env::var("ZELLIJ_PANE_ID")
        && let Err(message) = zellij::rename_tab_of(&pane, zellij::TAB_NAME)
    {
        app.notify(message);
    }
    let result = app.run(&mut terminal, &rx);
    ratatui::restore();
    if let Err(err) = result {
        eprintln!("agz: {err}");
        std::process::exit(1);
    }
}

impl App {
    fn run(&mut self, terminal: &mut ratatui::DefaultTerminal, rx: &Receiver<Event>) -> std::io::Result<()> {
        loop {
            terminal.draw(|f| {
                ui::draw(f, &self.model, &self.rows, self.own_zellij.as_deref(), &mut self.list, self.mirror.as_ref());
            })?;

            let first = match self.wake_after() {
                None => rx.recv().ok(),
                Some(timeout) => match rx.recv_timeout(timeout) {
                    Ok(event) => Some(event),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return Ok(()),
                },
            };
            // Les rafales d'événements fichiers sont fusionnées en une seule relecture.
            let mut fs = false;
            let mut head = false;
            let mut pins = false;
            for event in first.into_iter().chain(rx.try_iter()) {
                match event {
                    Event::Fs => fs = true,
                    Event::Head => head = true,
                    Event::Pins => pins = true,
                    Event::Git(cwd, repo) => self.apply_git(cwd, repo),
                    Event::ProcExit(pid) => self.proc_exit(pid),
                    Event::Pane(generation, update) => {
                        if let Some(mirror) = &mut self.mirror {
                            mirror.apply(generation, update);
                        }
                    }
                    Event::Usage(Ok(accounts)) => {
                        self.model.usage = accounts;
                        self.model.usage_error = None;
                    }
                    Event::Usage(Err(message)) => self.model.usage_error = Some(message),
                    Event::Resize => {}
                    Event::Key(key) => {
                        if !self.key(key) {
                            return Ok(());
                        }
                    }
                }
            }
            if fs {
                self.rescan();
            }
            if pins {
                self.load_pins();
            }
            let headers = self.refresh_headers();
            if fs || head || pins || headers {
                self.request_git();
            }
            if model::any_working(&self.model, &self.rows) {
                self.model.tick = self.model.tick.wrapping_add(1);
            }
            if self.model.message.as_ref().is_some_and(|(_, at)| at.elapsed() >= MESSAGE_TTL) {
                self.model.message = None;
            }
            self.rebuild();
            self.sync_mirror();
        }
    }

    /// Délai avant le prochain réveil sans événement : frame du spinner, fin du message, ou minute
    /// suivante pour les comptes à rebours des jauges.
    fn wake_after(&self) -> Option<Duration> {
        let spinner = model::any_working(&self.model, &self.rows).then_some(SPINNER_FRAME);
        let message = self.model.message.as_ref().map(|(_, at)| MESSAGE_TTL.saturating_sub(at.elapsed()));
        let minute = (!self.model.usage.is_empty()).then(|| Duration::from_millis(60_000 - usage::now_ms() % 60_000));
        [spinner, message, minute].into_iter().flatten().min()
    }

    /// Fait suivre à la copie ouverte la ligne sélectionnée.
    fn sync_mirror(&mut self) {
        let Some(mirror) = &mut self.mirror else { return };
        let row = self.model.selected.as_ref().and_then(|k| self.rows.iter().find(|r| &r.key == k));
        match model::copy_source(&self.model, row) {
            CopySource::Pane(z) => mirror.follow(&z.session, &z.pane_id),
            CopySource::Message(message) => mirror.show(message),
        }
    }

    fn rebuild(&mut self) {
        let after = model::rows(&self.model);
        model::select_after_rebuild(&mut self.model, &self.rows, &after);
        self.rows = after;
    }

    /// Relit le dossier d'exécution : une instance existe si son fichier est là et son pid vivant.
    fn rescan(&mut self) {
        let mut seen: HashSet<Key> = HashSet::new();
        let mut errors = BTreeMap::new();
        let mut turn_ended = false;
        for entry in std::fs::read_dir(&self.run_dir).into_iter().flatten().flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else { continue };
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            if !self.model.watched.contains(&pid) {
                if !proc::is_bun(pid) {
                    // Mort ou pid réutilisé : reste d'un crash.
                    let _ = std::fs::remove_dir_all(&dir);
                    continue;
                }
                match proc::watch_exit(pid, self.tx.clone()) {
                    Ok(()) => {
                        self.model.watched.insert(pid);
                    }
                    Err(err) => self.notify(format!("pid {pid} : surveillance impossible ({err})")),
                }
            }
            for file in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let name = file.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') || !name.ends_with(".json") {
                    continue;
                }
                match record::read(&file.path(), pid) {
                    None => {}
                    Some(Parsed::Error { pid, message }) => {
                        errors.insert(pid, message);
                    }
                    Some(Parsed::Ok(mut rec)) => {
                        // Le dossier fait foi : c'est lui qui est supprimé à la mort du pid.
                        rec.pid = pid;
                        let key = (pid, rec.agent.id.clone());
                        seen.insert(key.clone());
                        // Une racine qui cesse de travailler a fini un tour : la consommation a changé.
                        let was_working = self.model.instances.get(&key).is_some_and(|r| r.state == State::Working);
                        turn_ended |= was_working && rec.agent.kind == Kind::Main && rec.state != State::Working;
                        self.model.instances.insert(key, rec);
                    }
                }
            }
        }
        self.model.instances.retain(|key, _| seen.contains(key));
        self.model.errors = errors;
        self.model.retain_repos();
        if turn_ended {
            let _ = self.usage.send(());
        }
    }

    /// Relit `session-pins.json`.
    fn load_pins(&mut self) {
        self.model.pins = pins::load(&self.agent_dir);
    }

    /// Relit les en-têtes des sessions épinglées non publiées quand leur ensemble a changé (épingles
    /// modifiées, instance apparue ou disparue) : une session n'a de fichier qu'après sa première
    /// écriture, l'en-tête n'est donc lu qu'au moment où elle devient dormante. Vrai si relu.
    fn refresh_headers(&mut self) -> bool {
        if self.model.unpublished_pins().eq(self.headers_for.iter()) {
            return false;
        }
        self.headers_for = self.model.unpublished_pins().cloned().collect();
        self.model.headers = self.headers_for.iter().filter_map(|id| pins::header(&self.agent_dir, id)).collect();
        self.model.retain_repos();
        true
    }

    /// Demande le statut git de tous les dossiers affichés (calcul immédiat sur le thread dédié).
    fn request_git(&self) {
        let _ = self.git.send(self.model.cwds().into_iter().map(Path::to_path_buf).collect());
    }

    fn apply_git(&mut self, cwd: PathBuf, repo: Option<git::Repo>) {
        if let Some(git_dir) = repo.as_ref().map(|r| &r.git_dir)
            && !self.git_dirs.contains(git_dir)
        {
            match watch::watch_git_dir(&mut self.watcher, git_dir) {
                Ok(()) => {
                    self.git_dirs.insert(git_dir.clone());
                }
                Err(err) => self.notify(format!("{} : {err}", git_dir.display())),
            }
        }
        self.model.repos.insert(cwd, repo);
    }

    fn proc_exit(&mut self, pid: i32) {
        let _ = std::fs::remove_dir_all(self.run_dir.join(pid.to_string()));
        self.model.instances.retain(|(p, _), _| *p != pid);
        self.model.errors.remove(&pid);
        self.model.watched.remove(&pid);
        self.model.folded.retain(|(p, _)| *p != pid);
    }

    fn notify(&mut self, message: String) {
        self.model.message = Some((message, Instant::now()));
    }

    /// Traite une touche ; `false` pour quitter.
    fn key(&mut self, key: KeyEvent) -> bool {
        let index = self.model.selected.as_ref().and_then(|k| self.rows.iter().position(|r| &r.key == k));
        // Une confirmation de fermeture ne vaut que pour la touche suivante, dans les 3 s.
        let pending = self.close_pending.take().filter(|(_, at)| at.elapsed() < MESSAGE_TTL);
        if pending.is_some() && key.code != KeyCode::Char('x') {
            self.model.message = None;
        }
        match key.code {
            KeyCode::Char('q') => return false,
            KeyCode::Char('d') => {
                self.mirror = match self.mirror.take() {
                    Some(_) => None,
                    None => Some(Mirror::new(self.tx.clone())),
                };
            }
            KeyCode::Char('x') => {
                let Some(row) = index.map(|i| &self.rows[i]) else { return true };
                if !matches!(row.kind, RowKind::Root | RowKind::Sub { .. }) {
                    return true;
                }
                // Un sous-agent partage le pane de sa racine.
                let root_key = (row.key.0, ROOT_ID.to_string());
                let Some(root) = self.model.instances.get(&root_key) else { return true };
                let own = self.own_zellij.as_deref();
                if pending.is_some_and(|(key, _)| key == root_key) {
                    self.model.message = None;
                    if let Err(message) = zellij::close(own, root.zellij.as_ref()) {
                        self.notify(message);
                    }
                } else if let Err(message) = zellij::check_close(own, root.zellij.as_ref()) {
                    self.notify(message);
                } else {
                    let title = if root.title.is_empty() { "cette session" } else { root.title.as_str() };
                    self.notify(format!("Fermer “{title}” ? x pour confirmer"));
                    self.close_pending = Some((root_key, Instant::now()));
                }
            }
            KeyCode::Esc => self.mirror = None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(i) = index.filter(|&i| i > 0) {
                    self.model.selected = Some(self.rows[i - 1].key.clone());
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(row) = index.and_then(|i| self.rows.get(i + 1)) {
                    self.model.selected = Some(row.key.clone());
                }
            }
            KeyCode::Char(' ') => {
                if let Some(row) = index.map(|i| &self.rows[i]).filter(|r| r.kind == RowKind::Root) {
                    let key = row.key.clone();
                    if !self.model.folded.remove(&key) {
                        self.model.folded.insert(key);
                    }
                }
            }
            KeyCode::Char('p') => {
                let Some(id) = index.and_then(|i| model::session_of(&self.model, &self.rows[i])) else { return true };
                match pins::toggle(&self.agent_dir, &id) {
                    // Sans attendre l'événement du watcher, qui relira le même fichier.
                    Ok(_) => self.load_pins(),
                    Err(message) => self.notify(message),
                }
            }
            KeyCode::Enter => {
                let Some(row) = index.map(|i| &self.rows[i]) else { return true };
                let result = if let Some(p) = self.model.dormant(&row.key) {
                    if self.model.is_live(&p.id) || pins::lease_held(&self.omp_dir, &p.id) {
                        Err("Session déjà ouverte par un autre omp".to_string())
                    } else {
                        let cwd = if p.cwd.is_dir() { &p.cwd } else { &self.home };
                        zellij::resume(self.own_zellij.as_deref(), &p.id, cwd)
                    }
                } else {
                    // Un sous-agent partage le pane de sa racine.
                    let Some(root) = self.model.instances.get(&(row.key.0, ROOT_ID.to_string())) else { return true };
                    zellij::focus(self.own_zellij.as_deref(), root.zellij.as_ref())
                };
                if let Err(message) = result {
                    self.notify(message);
                }
            }
            _ => {}
        }
        true
    }
}
