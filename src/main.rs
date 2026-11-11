//! xst : tableau de bord des instances omp interactives et de leurs sous-agents.
//!
//! L'extension xs-tui (`extension/`) publie l'état de chaque agent dans `~/.omp/run/xs-tui/<pid>/` ;
//! ce binaire surveille ces fichiers, la vie des pids et les HEAD git, et redessine à chaque
//! changement (voir SPEC.md).

mod filter;
mod git;
mod mirror;
mod model;
mod pins;
mod proc;
mod prompt;
mod record;
mod ui;
mod usage;
mod watch;
mod zellij;

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste, KeyCode, KeyEvent, KeyModifiers};
use notify::RecommendedWatcher;
use ratatui::widgets::ListState;

use mirror::Mirror;
use model::{CopySource, Key, Model, ROOT_ID, Row, RowKind};
use pins::Pinned;
use prompt::Prompt;
use record::{Kind, Parsed, State};
use watch::Event;
use zellij::Headless;

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
    /// Plugin zellij des sessions headless, ou la raison de son absence.
    plugin: Result<zellij::Plugin, String>,
    /// Demandes au thread qui relit les panes headless.
    headless: Sender<()>,
    /// Prompt en cours de saisie, ouvert par `w` (SPEC.md § Envoi de prompt).
    prompt: Option<Prompt>,
}

fn main() {
    let Some(home) = std::env::var_os("HOME") else {
        eprintln!("xst: HOME non défini");
        std::process::exit(1);
    };
    let home = PathBuf::from(home);
    let omp_dir = home.join(".omp");
    let agent_dir = omp_dir.join("agent");
    let run_dir = omp_dir.join("run/xs-tui");
    let (tx, rx) = mpsc::channel();
    let watcher = match watch::start(&run_dir, &agent_dir, tx.clone()) {
        Ok(w) => w,
        Err(err) => {
            eprintln!("xst: surveillance de {} impossible : {err}", run_dir.display());
            std::process::exit(1);
        }
    };
    let headless = zellij::spawn_headless_refresher(tx.clone());
    let own_zellij = std::env::var("ZELLIJ_SESSION_NAME").ok();
    let plugin = match own_zellij {
        None => Err("xst hors zellij : arrière-plan impossible".to_string()),
        Some(_) => zellij::Plugin::install(&home.join(".cache/xst"), headless.clone())
            .map_err(|err| format!("plugin xs-tui : {err}")),
    };
    let mut app = App {
        model: Model::default(),
        rows: Vec::new(),
        list: ListState::default(),
        run_dir,
        omp_dir,
        agent_dir,
        home,
        own_zellij,
        tx: tx.clone(),
        watcher,
        git_dirs: HashSet::new(),
        git: git::spawn_refresher(tx.clone()),
        headers_for: Vec::new(),
        mirror: None,
        usage: usage::spawn_refresher(tx.clone()),
        close_pending: None,
        plugin,
        headless,
        prompt: None,
    };

    let mut terminal = ratatui::init();
    // Un texte collé arrive en un bloc : ses retours à la ligne n'envoient pas le prompt.
    let _ = crossterm::execute!(std::io::stdout(), EnableBracketedPaste);
    watch::spawn_terminal_reader(tx);
    app.rescan();
    app.load_pins();
    app.refresh_headers();
    app.request_git();
    app.request_headless();
    app.rebuild();
    // Seulement au démarrage : la fonction fish de l'onglet remet son nom à la commande suivante.
    if app.own_zellij.is_some()
        && let Ok(pane) = std::env::var("ZELLIJ_PANE_ID")
        && let Err(message) = zellij::rename_tab_of(&pane, zellij::TAB_NAME)
    {
        app.notify(message);
    }
    let result = app.run(&mut terminal, &rx);
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    if let Err(err) = result {
        eprintln!("xst: {err}");
        std::process::exit(1);
    }
}

impl App {
    fn run(&mut self, terminal: &mut ratatui::DefaultTerminal, rx: &Receiver<Event>) -> std::io::Result<()> {
        loop {
            terminal.draw(|f| {
                ui::draw(
                    f,
                    &self.model,
                    &self.rows,
                    self.own_zellij.as_deref(),
                    &mut self.list,
                    self.mirror.as_ref(),
                    self.prompt.as_ref(),
                );
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
                    Event::Notice(message) => self.notify(message),
                    Event::Headless(panes) => self.model.headless = panes,
                    Event::Resize => {}
                    Event::Paste(text) => {
                        if let Some(prompt) = &mut self.prompt {
                            prompt.paste(&text);
                        } else if self.model.filtering {
                            // Le filtre tient sur une ligne.
                            self.model.filter.extend(text.chars().map(|c| if c.is_control() { ' ' } else { c }));
                            self.refilter();
                        }
                    }
                    Event::Key(key) if self.prompt.is_some() => self.prompt_key(key),
                    Event::Key(key) if self.model.filtering => self.filter_key(key),
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

    /// Reconstruit la liste après un changement de la requête du filtre : comme dans fzf, le curseur
    /// passe à la première ligne, la plus pertinente.
    fn refilter(&mut self) {
        let after = model::rows(&self.model);
        self.model.selected = after.first().map(|r| r.key.clone());
        model::select_after_rebuild(&mut self.model, &self.rows, &after);
        self.rows = after;
    }

    /// Monte ou descend le curseur d'une ligne.
    fn step(&mut self, down: bool) {
        let Some(i) = self.model.selected.as_ref().and_then(|k| self.rows.iter().position(|r| &r.key == k)) else {
            return;
        };
        let next = if down { i.checked_add(1) } else { i.checked_sub(1) };
        if let Some(row) = next.and_then(|n| self.rows.get(n)) {
            self.model.selected = Some(row.key.clone());
        }
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
        // Une instance apparue, disparue ou déplacée : son pane a pu changer d'état.
        self.request_headless();
    }

    /// Demande la relecture des panes headless, dans zellij seulement.
    fn request_headless(&self) {
        if self.own_zellij.is_some() {
            let _ = self.headless.send(());
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
                } else if let Err(message) = zellij::check_target(own, root.zellij.as_ref()) {
                    self.notify(message);
                } else {
                    let title = if root.title.is_empty() { "cette session" } else { root.title.as_str() };
                    // Sans fichier de session, rien à reprendre : l'épingle sera ignorée une fois omp fermé.
                    let empty = self.model.is_pinned(&root.session_id)
                        && !pins::has_session_file(&self.agent_dir, &root.session_id);
                    self.notify(if empty {
                        format!("“{title}” est vide : son épingle sera perdue. x pour fermer")
                    } else {
                        format!("Fermer “{title}” ? x pour confirmer")
                    });
                    self.close_pending = Some((root_key, Instant::now()));
                }
            }
            KeyCode::Char('f') => {
                self.model.filtering = true;
                self.model.message = None;
            }
            // Le filtre d'abord : la copie reste ouverte sur la session trouvée.
            KeyCode::Esc if !self.model.filter.is_empty() => self.model.filter.clear(),
            KeyCode::Esc => self.mirror = None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return false,
            KeyCode::Up | KeyCode::Char('k') => self.step(false),
            KeyCode::Down | KeyCode::Char('j') => self.step(true),
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
                match pins::toggle(&self.agent_dir, id) {
                    // Sans attendre l'événement du watcher, qui relira le même fichier.
                    Ok(_) => self.load_pins(),
                    Err(message) => self.notify(message),
                }
            }
            KeyCode::Enter => {
                let Some(row) = index.map(|i| &self.rows[i]) else { return true };
                let own = self.own_zellij.as_deref();
                let result = if let Some(p) = self.model.dormant(&row.key) {
                    self.dormant_cwd(p).and_then(|cwd| zellij::resume(own, &p.id, cwd))
                } else {
                    // Un sous-agent partage le pane de sa racine.
                    let Some(root) = self.model.instances.get(&(row.key.0, ROOT_ID.to_string())) else { return true };
                    match root.zellij.as_ref() {
                        // Un pane headless n'est dans aucun onglet : le plugin le sort dans le sien.
                        Some(z) if self.model.is_headless(own, Some(z)) => self.headless(Headless::Show(&z.pane_id)),
                        target => zellij::focus(own, target),
                    }
                };
                if let Err(message) = result {
                    self.notify(message);
                }
            }
            KeyCode::Char('w') => {
                let Some(row) = index.map(|i| &self.rows[i]) else { return true };
                match row.kind {
                    RowKind::Error => {}
                    RowKind::Dormant => self.notify("Session dormante : s pour la reprendre en arrière-plan".to_string()),
                    RowKind::Root | RowKind::Sub { .. } => {
                        // Un sous-agent n'a pas d'éditeur : le prompt va à sa racine.
                        let pid = row.key.0;
                        let Some(root) = self.model.instances.get(&(pid, ROOT_ID.to_string())) else { return true };
                        self.prompt = Some(Prompt::new(pid, root.title.clone()));
                        self.model.message = None;
                    }
                }
            }
            KeyCode::Char('s') => {
                let Some(row) = index.map(|i| &self.rows[i]) else { return true };
                let message = match self.model.dormant(&row.key) {
                    Some(p) => {
                        let resumed = self.dormant_cwd(p).and_then(|cwd| self.headless(Headless::Resume { id: &p.id, cwd }));
                        resumed.map_or_else(|message| message, |()| format!("{} reprise en arrière-plan", label(&p.title)))
                    }
                    None if row.kind == RowKind::Error => return true,
                    None => {
                        let root = self.model.instances.get(&(row.key.0, ROOT_ID.to_string()));
                        if root.is_some_and(|r| self.model.is_headless(self.own_zellij.as_deref(), r.zellij.as_ref())) {
                            "Session déjà en arrière-plan".to_string()
                        } else {
                            "Session déjà ouverte : D pour la passer en arrière-plan".to_string()
                        }
                    }
                };
                self.notify(message);
            }
            KeyCode::Char('D') => {
                let Some(row) = index.map(|i| &self.rows[i]) else { return true };
                let message = match row.kind {
                    RowKind::Error => return true,
                    RowKind::Dormant => "Session dormante : s pour la reprendre en arrière-plan".to_string(),
                    RowKind::Root | RowKind::Sub { .. } => {
                        // Un sous-agent partage le pane de sa racine.
                        let Some(root) = self.model.instances.get(&(row.key.0, ROOT_ID.to_string())) else { return true };
                        let own = self.own_zellij.as_deref();
                        if self.model.is_headless(own, root.zellij.as_ref()) {
                            "Session déjà en arrière-plan".to_string()
                        } else {
                            let hidden = zellij::check_target(own, root.zellij.as_ref())
                                .and_then(|z| self.headless(Headless::Hide(&z.pane_id)));
                            hidden.map_or_else(|message| message, |()| format!("{} passée en arrière-plan", label(&root.title)))
                        }
                    }
                };
                self.notify(message);
            }
            _ => {}
        }
        true
    }

    /// Touche pendant la saisie du filtre : la liste suit chaque caractère, les flèches déplacent le
    /// curseur dans la liste filtrée.
    fn filter_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let filter = &mut self.model.filter;
        match key.code {
            // Le curseur reste sur sa ligne, que la liste complète contient : la boucle la reconstruit.
            KeyCode::Esc => {
                filter.clear();
                self.model.filtering = false;
                return;
            }
            KeyCode::Char('c') if ctrl => {
                filter.clear();
                self.model.filtering = false;
                return;
            }
            // Garde le filtre et rend la main aux commandes, qui portent sur la liste filtrée.
            KeyCode::Enter => {
                if filter.trim().is_empty() {
                    filter.clear();
                }
                self.model.filtering = false;
                return;
            }
            KeyCode::Up => return self.step(false),
            KeyCode::Char('p' | 'k') if ctrl => return self.step(false),
            KeyCode::Down => return self.step(true),
            KeyCode::Char('n' | 'j') if ctrl => return self.step(true),
            KeyCode::Backspace => {
                filter.pop();
            }
            KeyCode::Char('u') if ctrl => filter.clear(),
            KeyCode::Char(c) if !ctrl => filter.push(c),
            _ => return,
        }
        self.refilter();
    }

    /// Touche pendant la saisie d'un prompt : les autres commandes attendent sa fin.
    fn prompt_key(&mut self, key: KeyEvent) {
        let Some(prompt) = &mut self.prompt else { return };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.prompt = None,
            KeyCode::Char('c') if ctrl => self.prompt = None,
            KeyCode::Char('u') if ctrl => prompt.text.clear(),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => prompt.text.push('\n'),
            KeyCode::Enter => {
                if let Some(prompt) = self.prompt.take() {
                    self.send_prompt(prompt);
                }
            }
            KeyCode::Backspace => {
                prompt.text.pop();
            }
            KeyCode::Char(c) if !ctrl => prompt.text.push(c),
            _ => {}
        }
    }

    /// Dépose le prompt saisi pour son instance ; un prompt vide ferme la saisie sans rien envoyer.
    fn send_prompt(&mut self, prompt: Prompt) {
        if prompt.text.trim().is_empty() {
            return;
        }
        let message = if self.model.instances.contains_key(&(prompt.pid, ROOT_ID.to_string())) {
            match prompt::send(&self.run_dir, prompt.pid, &prompt.text) {
                Ok(()) if prompt.title.is_empty() => "Prompt envoyé".to_string(),
                Ok(()) => format!("Prompt envoyé à “{}”", prompt.title),
                Err(message) => message,
            }
        } else {
            format!("{} fermée : prompt non envoyé", label(&prompt.title))
        };
        self.notify(message);
    }

    /// Dossier où reprendre la session dormante `p` (`$HOME` s'il a disparu), si aucun omp ne l'a
    /// déjà ouverte.
    fn dormant_cwd<'a>(&'a self, p: &'a Pinned) -> Result<&'a Path, String> {
        if self.model.is_live(&p.id) || pins::lease_held(&self.omp_dir, &p.id) {
            return Err("Session déjà ouverte par un autre omp".to_string());
        }
        Ok(if p.cwd.is_dir() { &p.cwd } else { &self.home })
    }

    /// Envoie `command` au plugin xs-tui ; sa réponse arrive plus tard (`Event::Notice`).
    fn headless(&self, command: Headless) -> Result<(), String> {
        self.plugin.as_ref().map_err(Clone::clone)?.send(command, &self.tx);
        Ok(())
    }
}

/// Titre d'une session pour un message : `“<titre>”`, ou « Session » sans titre.
fn label(title: &str) -> String {
    if title.is_empty() { "Session".to_string() } else { format!("“{title}”") }
}
