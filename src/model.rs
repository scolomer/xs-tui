//! État affiché : instances connues, lignes d'erreur, pliage et curseur ; construction de l'arbre.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::git::Repo;
use crate::pins::Pinned;
use crate::record::{Kind, Record, State, Zellij};

/// Identité d'une ligne : (pid, `agent.id`). Une ligne d'erreur a un id vide ; une session
/// dormante a le pid `DORMANT_PID` et son id de session.
pub type Key = (i32, String);

/// pid des clés de sessions dormantes : aucun process n'a le pid 0.
pub const DORMANT_PID: i32 = 0;

/// Id de l'agent racine d'un process omp.
pub const ROOT_ID: &str = "Main";

#[derive(Default)]
pub struct Model {
    pub instances: HashMap<Key, Record>,
    /// Dépôt git par `cwd` d'instance ou de session épinglée ; absent tant que le premier statut
    /// n'est pas calculé, `None` hors dépôt.
    pub repos: HashMap<PathBuf, Option<Repo>>,
    /// Ids épinglés, dans l'ordre de `session-pins.json`.
    pub pins: Vec<String>,
    /// En-têtes des épingles non vivantes dont le fichier de session existe, dans l'ordre des
    /// épingles.
    pub headers: Vec<Pinned>,
    /// pid -> message ; la ligne d'erreur remplace les lignes du pid.
    pub errors: BTreeMap<i32, String>,
    /// pids dont la sortie est surveillée.
    pub watched: HashSet<i32>,
    /// Racines repliées.
    pub folded: HashSet<Key>,
    pub selected: Option<Key>,
    /// Session de la ligne sélectionnée au dernier `select_after_rebuild`, avec la clé de cette
    /// ligne : le curseur suit la session quand sa ligne change de forme (reprise, fermeture).
    pub selected_session: Option<(Key, String)>,
    /// Message temporaire de la barre du bas.
    pub message: Option<(String, Instant)>,
    /// Frame du spinner.
    pub tick: usize,
    /// Jauges de consommation, dans l'ordre d'affichage ; gardées après un échec de relecture.
    pub usage: Vec<crate::usage::Account>,
    /// Dernier échec de relecture de la consommation.
    pub usage_error: Option<String>,
    /// Ids des panes headless (« supprimés » par zellij) de la session zellij d'xst.
    pub headless: HashSet<String>,
}

impl Model {
    /// Dépôt du dossier `cwd`, s'il est connu et que `cwd` est dans un dépôt.
    pub fn repo(&self, cwd: &Path) -> Option<&Repo> {
        self.repos.get(cwd).and_then(Option::as_ref)
    }

    /// Vrai si la session `session_id` est épinglée.
    pub fn is_pinned(&self, session_id: &str) -> bool {
        self.pins.iter().any(|id| id == session_id)
    }

    /// Vrai si `target`, pane d'une instance, est headless dans la session zellij `own` d'xst.
    pub fn is_headless(&self, own: Option<&str>, target: Option<&Zellij>) -> bool {
        target.is_some_and(|z| own == Some(z.session.as_str()) && self.headless.contains(&z.pane_id))
    }

    /// Ids épinglés qu'aucune instance ne publie, dans l'ordre des épingles.
    pub fn unpublished_pins(&self) -> impl Iterator<Item = &String> {
        self.pins.iter().filter(|id| !self.is_live(id))
    }

    /// Session épinglée d'une ligne dormante.
    pub fn dormant(&self, key: &Key) -> Option<&Pinned> {
        if key.0 != DORMANT_PID {
            return None;
        }
        self.headers.iter().find(|p| p.id == key.1)
    }

    /// Vrai si une instance publie la session `session_id`.
    pub fn is_live(&self, session_id: &str) -> bool {
        self.instances.values().any(|r| r.agent.kind == Kind::Main && r.session_id == session_id)
    }

    /// Dossiers dont le statut git est affiché : instances et sessions dormantes.
    pub fn cwds(&self) -> HashSet<&Path> {
        cwds(&self.instances, &self.headers)
    }

    /// Oublie le statut git des dossiers qui ne sont plus affichés.
    pub fn retain_repos(&mut self) {
        let cwds = cwds(&self.instances, &self.headers);
        self.repos.retain(|cwd, _| cwds.contains(cwd.as_path()));
    }
}

fn cwds<'a>(instances: &'a HashMap<Key, Record>, headers: &'a [Pinned]) -> HashSet<&'a Path> {
    let instances = instances.values().map(|r| r.cwd.as_path());
    instances.chain(headers.iter().map(|p| p.cwd.as_path())).collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowKind {
    Root,
    Sub { show_location: bool },
    Error,
    Dormant,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub key: Key,
    pub kind: RowKind,
    /// Ligne parente affichée (racine ou sous-agent) ; `None` pour une racine, une session
    /// dormante ou une erreur.
    pub parent: Option<Key>,
    /// Sous-agents masqués par le pliage d'une racine.
    pub hidden_children: usize,
    /// Guides d'arbre d'un sous-agent (`  │ └ `) ; vide pour les autres lignes.
    pub tree: String,
}

/// Lignes à afficher, dans l'ordre : racines par `startedAt`, chacune suivie de ses sous-agents,
/// puis les sessions dormantes dans l'ordre des épingles, puis les lignes d'erreur par pid.
pub fn rows(m: &Model) -> Vec<Row> {
    let mut roots: Vec<&Record> = m
        .instances
        .values()
        .filter(|r| r.agent.kind == Kind::Main && r.agent.id == ROOT_ID && !m.errors.contains_key(&r.pid))
        .collect();
    roots.sort_by(|a, b| (&a.started_at, a.pid).cmp(&(&b.started_at, b.pid)));

    let mut out = Vec::new();
    for root in roots {
        let pid = root.pid;
        let root_key = (pid, ROOT_ID.to_string());
        let subs: HashMap<&str, &Record> = m
            .instances
            .iter()
            .filter(|((p, _), r)| *p == pid && r.agent.kind == Kind::Sub)
            .map(|((_, id), r)| (id.as_str(), r))
            .collect();
        let folded = m.folded.contains(&root_key);
        out.push(Row {
            key: root_key.clone(),
            kind: RowKind::Root,
            parent: None,
            hidden_children: if folded { subs.len() } else { 0 },
            tree: String::new(),
        });
        if folded {
            continue;
        }
        let mut children: HashMap<&str, Vec<&Record>> = HashMap::new();
        for (id, sub) in &subs {
            children.entry(attach_point(id, &subs)).or_default().push(sub);
        }
        for list in children.values_mut() {
            list.sort_by(|a, b| (&a.started_at, &a.agent.id).cmp(&(&b.started_at, &b.agent.id)));
        }
        push_children(&mut out, pid, root, ROOT_ID, "  ", &children);
    }
    out.extend(m.headers.iter().filter(|p| !m.is_live(&p.id)).map(|p| Row {
        key: (DORMANT_PID, p.id.clone()),
        kind: RowKind::Dormant,
        parent: None,
        hidden_children: 0,
        tree: String::new(),
    }));
    out.extend(m.errors.keys().map(|&pid| Row {
        key: (pid, String::new()),
        kind: RowKind::Error,
        parent: None,
        hidden_children: 0,
        tree: String::new(),
    }));
    out
}

/// Parent affiché d'un sous-agent : son `parentId` si la chaîne remonte jusqu'à la racine du pid,
/// sinon la racine (parent inconnu ou disparu, ou cycle).
fn attach_point<'a>(id: &'a str, subs: &HashMap<&'a str, &'a Record>) -> &'a str {
    let parent_of = |id: &str| subs.get(id).and_then(|r| r.agent.parent_id.as_deref());
    let Some(parent) = parent_of(id) else { return ROOT_ID };
    let mut cur = parent;
    for _ in 0..=subs.len() {
        if cur == ROOT_ID {
            return parent;
        }
        if !subs.contains_key(cur) {
            return ROOT_ID;
        }
        match parent_of(cur) {
            Some(next) => cur = next,
            None => return ROOT_ID,
        }
    }
    ROOT_ID
}

/// Ajoute les sous-agents de `parent_id` ; `guides` est le préfixe hérité des ancêtres : `│ `
/// pour un ancêtre qui a encore des frères en dessous, deux espaces sinon.
fn push_children(
    out: &mut Vec<Row>,
    pid: i32,
    parent: &Record,
    parent_id: &str,
    guides: &str,
    children: &HashMap<&str, Vec<&Record>>,
) {
    let Some(list) = children.get(parent_id) else { return };
    for (i, child) in list.iter().enumerate() {
        let last = i + 1 == list.len();
        let id = child.agent.id.as_str();
        out.push(Row {
            key: (pid, id.to_string()),
            kind: RowKind::Sub { show_location: child.cwd != parent.cwd },
            parent: Some((pid, parent_id.to_string())),
            hidden_children: 0,
            tree: format!("{guides}{}", if last { "└ " } else { "├ " }),
        });
        let guides = format!("{guides}{}", if last { "  " } else { "│ " });
        push_children(out, pid, child, id, &guides, children);
    }
}

/// Garde le curseur sur l'élément sélectionné. Si sa ligne a disparu, suit sa session sous son autre
/// forme (session dormante reprise, session épinglée fermée), sinon passe au plus proche ancêtre
/// encore affiché, sinon à la ligne voisine.
pub fn select_after_rebuild(m: &mut Model, before: &[Row], after: &[Row]) {
    reselect(m, before, after);
    m.selected_session = m.selected.as_ref().and_then(|key| {
        let row = after.iter().find(|r| &r.key == key)?;
        Some((key.clone(), session_of(m, row)?.to_owned()))
    });
}

fn reselect(m: &mut Model, before: &[Row], after: &[Row]) {
    let Some(selected) = m.selected.clone() else {
        m.selected = after.first().map(|r| r.key.clone());
        return;
    };
    let present = |key: &Key| after.iter().any(|r| &r.key == key);
    if present(&selected) {
        return;
    }
    // Une session dormante a son id pour clé ; celle d'une instance est notée au passage précédent.
    let session = match &m.selected_session {
        _ if selected.0 == DORMANT_PID => Some(selected.1.as_str()),
        Some((key, session)) if *key == selected => Some(session.as_str()),
        _ => None,
    };
    let moved = session.and_then(|session| {
        after
            .iter()
            .find(|r| matches!(r.kind, RowKind::Root | RowKind::Dormant) && session_of(m, r) == Some(session))
    });
    if let Some(row) = moved {
        m.selected = Some(row.key.clone());
        return;
    }
    let find_before = |key: &Key| before.iter().position(|r| &r.key == key);
    let mut ancestor = find_before(&selected).and_then(|i| before[i].parent.clone());
    while let Some(key) = ancestor {
        if present(&key) {
            m.selected = Some(key);
            return;
        }
        ancestor = find_before(&key).and_then(|i| before[i].parent.clone());
    }
    let index = find_before(&selected).unwrap_or(0);
    m.selected = after.get(index.min(after.len().saturating_sub(1))).map(|r| r.key.clone());
}

/// Vrai si une ligne affichée est en cours de travail : le spinner doit être animé.
pub fn any_working(m: &Model, rows: &[Row]) -> bool {
    rows.iter().any(|r| m.instances.get(&r.key).is_some_and(|rec| rec.state == State::Working))
}

/// Ce que montre la copie pour la ligne sélectionnée (SPEC.md § Copie de session).
#[derive(Debug, PartialEq, Eq)]
pub enum CopySource<'a> {
    Pane(&'a Zellij),
    Message(&'static str),
}

/// Source de la copie : un sous-agent montre le pane de sa racine.
pub fn copy_source<'a>(m: &'a Model, row: Option<&Row>) -> CopySource<'a> {
    let Some(row) = row else { return CopySource::Message("Aucune session sélectionnée") };
    match row.kind {
        RowKind::Dormant => CopySource::Message("Session dormante"),
        RowKind::Error => CopySource::Message("Ligne d'erreur"),
        RowKind::Root | RowKind::Sub { .. } => {
            match m.instances.get(&(row.key.0, ROOT_ID.to_string())).and_then(|r| r.zellij.as_ref()) {
                Some(z) => CopySource::Pane(z),
                None => CopySource::Message("Instance hors zellij"),
            }
        }
    }
}

/// Session d'une ligne, celle que `p` épingle : un sous-agent compte pour sa racine ; aucune pour
/// une ligne d'erreur.
pub fn session_of<'a>(m: &'a Model, row: &'a Row) -> Option<&'a str> {
    match row.kind {
        RowKind::Dormant => Some(row.key.1.as_str()),
        RowKind::Error => None,
        RowKind::Root | RowKind::Sub { .. } => {
            m.instances.get(&(row.key.0, ROOT_ID.to_string())).map(|r| r.session_id.as_str())
        }
    }
}

/// Titre de la session de la ligne sélectionnée, pour la ligne de séparation de la copie.
pub fn copy_title<'a>(m: &'a Model, row: Option<&Row>) -> Option<&'a str> {
    let row = row?;
    match row.kind {
        RowKind::Dormant => m.dormant(&row.key).map(|p| p.title.as_str()),
        RowKind::Error => None,
        RowKind::Root | RowKind::Sub { .. } => {
            m.instances.get(&(row.key.0, ROOT_ID.to_string())).map(|r| r.title.as_str())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Agent;

    fn rec(pid: i32, id: &str, parent: Option<&str>, started: &str) -> Record {
        Record {
            pid,
            agent: Agent {
                kind: if parent.is_none() { Kind::Main } else { Kind::Sub },
                id: id.into(),
                name: "task".into(),
                parent_id: parent.map(Into::into),
            },
            session_id: "s".into(),
            cwd: "/tmp".into(),
            title: String::new(),
            state: State::Idle,
            zellij: None,
            started_at: started.into(),
        }
    }

    fn model(recs: Vec<Record>) -> Model {
        let mut m = Model::default();
        for rec in recs {
            let key = (rec.pid, rec.agent.id.clone());
            m.instances.insert(key, rec);
        }
        m
    }

    fn keys(rows: &[Row]) -> Vec<(i32, &str)> {
        rows.iter().map(|r| (r.key.0, r.key.1.as_str())).collect()
    }

    fn headers(ids: &[&str]) -> Vec<Pinned> {
        ids.iter().map(|id| Pinned { id: (*id).into(), cwd: "/tmp".into(), title: String::new() }).collect()
    }

    #[test]
    fn roots_are_ordered_by_start_time() {
        let m = model(vec![rec(1, "Main", None, "2026-01-02"), rec(2, "Main", None, "2026-01-01")]);
        assert_eq!(keys(&rows(&m)), [(2, "Main"), (1, "Main")]);
    }

    #[test]
    fn subagents_nest_and_orphans_attach_to_root() {
        let m = model(vec![
            rec(1, "Main", None, "a"),
            rec(1, "A", Some("Main"), "b"),
            rec(1, "A.1", Some("A"), "c"),
            rec(1, "Orphan", Some("Gone"), "d"),
        ]);
        let rows = rows(&m);
        assert_eq!(keys(&rows), [(1, "Main"), (1, "A"), (1, "A.1"), (1, "Orphan")]);
        // A a un frère après lui : sa branche se prolonge (│) jusqu'à Orphan, le dernier (└).
        let trees: Vec<&str> = rows.iter().map(|r| r.tree.as_str()).collect();
        assert_eq!(trees, ["", "  ├ ", "  │ └ ", "  └ "]);
    }

    #[test]
    fn guides_stop_under_a_last_child() {
        let m = model(vec![rec(1, "Main", None, "a"), rec(1, "A", Some("Main"), "b"), rec(1, "A.1", Some("A"), "c")]);
        let trees: Vec<String> = rows(&m).into_iter().map(|r| r.tree).collect();
        assert_eq!(trees, ["", "  └ ", "    └ "]);
    }

    #[test]
    fn pid_without_root_is_hidden() {
        let m = model(vec![rec(1, "A", Some("Main"), "a")]);
        assert!(rows(&m).is_empty());
    }

    #[test]
    fn folded_root_counts_hidden_subagents() {
        let mut m = model(vec![rec(1, "Main", None, "a"), rec(1, "A", Some("Main"), "b"), rec(1, "B", Some("A"), "c")]);
        m.folded.insert((1, "Main".into()));
        let rows = rows(&m);
        assert_eq!(keys(&rows), [(1, "Main")]);
        assert_eq!(rows[0].hidden_children, 2);
    }

    #[test]
    fn cursor_moves_to_parent_when_subagent_disappears() {
        let mut m = model(vec![rec(1, "Main", None, "a"), rec(1, "A", Some("Main"), "b"), rec(1, "B", Some("A"), "c")]);
        let before = rows(&m);
        m.selected = Some((1, "B".into()));
        m.instances.remove(&(1, "B".into()));
        let after = rows(&m);
        select_after_rebuild(&mut m, &before, &after);
        assert_eq!(m.selected, Some((1, "A".into())));
    }

    #[test]
    fn cursor_stays_in_bounds_when_last_row_disappears() {
        let mut m = model(vec![rec(1, "Main", None, "a"), rec(2, "Main", None, "b"), rec(3, "Main", None, "c")]);
        let before = rows(&m);
        m.selected = Some((3, "Main".into()));
        m.instances.remove(&(3, "Main".into()));
        let after = rows(&m);
        select_after_rebuild(&mut m, &before, &after);
        assert_eq!(m.selected, Some((2, "Main".into())));
    }

    #[test]
    fn dormant_sessions_follow_instances_in_pin_order() {
        let mut m = model(vec![rec(1, "Main", None, "a"), rec(1, "A", Some("Main"), "b")]);
        m.headers = headers(&["y", "s", "x"]);
        m.errors.insert(2, "illisible".into());
        // "s" est publiée par le pid 1 : vivante, sans ligne dormante.
        assert_eq!(keys(&rows(&m)), [(1, "Main"), (1, "A"), (0, "y"), (0, "x"), (2, "")]);
    }

    #[test]
    fn cursor_stays_in_bounds_when_last_dormant_disappears() {
        let mut m = model(vec![rec(1, "Main", None, "a")]);
        m.headers = headers(&["x", "y"]);
        let before = rows(&m);
        m.selected = Some((DORMANT_PID, "y".into()));
        m.headers.pop();
        let after = rows(&m);
        select_after_rebuild(&mut m, &before, &after);
        assert_eq!(m.selected, Some((DORMANT_PID, "x".into())));
    }

    #[test]
    fn subagent_copies_its_root_pane() {
        let mut root = rec(1, "Main", None, "a");
        root.zellij = Some(Zellij { session: "m1".into(), pane_id: "8".into() });
        let mut m = model(vec![root, rec(1, "A", Some("Main"), "b"), rec(2, "Main", None, "c")]);
        m.headers = headers(&["x"]);
        let rows = rows(&m);
        let source = |key: (i32, &str)| copy_source(&m, rows.iter().find(|r| (r.key.0, r.key.1.as_str()) == key));
        let pane = Zellij { session: "m1".into(), pane_id: "8".into() };
        assert_eq!(source((1, "A")), CopySource::Pane(&pane));
        assert_eq!(source((2, "Main")), CopySource::Message("Instance hors zellij"));
        assert_eq!(source((DORMANT_PID, "x")), CopySource::Message("Session dormante"));
    }

    #[test]
    fn cursor_follows_a_resumed_session_into_the_instances() {
        let mut m = model(vec![rec(1, "Main", None, "a")]);
        m.headers = headers(&["x", "y"]);
        let before = rows(&m);
        m.selected = Some((DORMANT_PID, "x".into()));
        let mut resumed = rec(2, "Main", None, "b");
        resumed.session_id = "x".into();
        m.instances.insert((2, "Main".into()), resumed);
        m.headers.remove(0);
        let after = rows(&m);
        select_after_rebuild(&mut m, &before, &after);
        assert_eq!(m.selected, Some((2, "Main".into())));
    }

    #[test]
    fn cursor_follows_a_closed_pinned_session_into_the_dormant_block() {
        let mut closed = rec(2, "Main", None, "b");
        closed.session_id = "x".into();
        let mut m = model(vec![rec(1, "Main", None, "a"), closed]);
        m.headers = headers(&["y"]);
        let before = rows(&m);
        m.selected = Some((2, "Main".into()));
        select_after_rebuild(&mut m, &before, &before);
        m.instances.remove(&(2, "Main".into()));
        // La voisine de l'instance fermée serait `y`.
        m.headers = headers(&["y", "x"]);
        let after = rows(&m);
        select_after_rebuild(&mut m, &before, &after);
        assert_eq!(m.selected, Some((DORMANT_PID, "x".into())));
    }
}
