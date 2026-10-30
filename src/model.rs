//! État affiché : instances connues, lignes d'erreur, pliage et curseur ; construction de l'arbre.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use crate::git::Repo;
use crate::record::{Kind, Record, State};

/// Identité d'une ligne : (pid, `agent.id`). Une ligne d'erreur a un id vide.
pub type Key = (i32, String);

/// Id de l'agent racine d'un process omp.
pub const ROOT_ID: &str = "Main";

#[derive(Default)]
pub struct Model {
    pub instances: HashMap<Key, Record>,
    /// Dépôt git par `cwd` d'instance ; absent tant que le premier statut n'est pas calculé,
    /// `None` hors dépôt.
    pub repos: HashMap<PathBuf, Option<Repo>>,
    /// pid -> message ; la ligne d'erreur remplace les lignes du pid.
    pub errors: BTreeMap<i32, String>,
    /// pids dont la sortie est surveillée.
    pub watched: HashSet<i32>,
    /// Racines repliées.
    pub folded: HashSet<Key>,
    pub selected: Option<Key>,
    /// Message temporaire de la barre du bas.
    pub message: Option<(String, Instant)>,
    /// Frame du spinner.
    pub tick: usize,
}

impl Model {
    /// Dépôt du dossier `cwd`, s'il est connu et que `cwd` est dans un dépôt.
    pub fn repo(&self, rec: &Record) -> Option<&Repo> {
        self.repos.get(&rec.cwd).and_then(Option::as_ref)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowKind {
    Root,
    Sub { depth: u32, show_location: bool },
    Error,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub key: Key,
    pub kind: RowKind,
    /// Ligne parente affichée (racine ou sous-agent) ; `None` pour une racine ou une erreur.
    pub parent: Option<Key>,
    /// Sous-agents masqués par le pliage d'une racine.
    pub hidden_children: usize,
}

/// Lignes à afficher, dans l'ordre : racines par `startedAt`, chacune suivie de ses sous-agents,
/// puis les lignes d'erreur par pid.
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
        push_children(&mut out, pid, root, ROOT_ID, 1, &children);
    }
    out.extend(m.errors.keys().map(|&pid| Row {
        key: (pid, String::new()),
        kind: RowKind::Error,
        parent: None,
        hidden_children: 0,
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

fn push_children(
    out: &mut Vec<Row>,
    pid: i32,
    parent: &Record,
    parent_id: &str,
    depth: u32,
    children: &HashMap<&str, Vec<&Record>>,
) {
    let Some(list) = children.get(parent_id) else { return };
    for child in list {
        let id = child.agent.id.as_str();
        out.push(Row {
            key: (pid, id.to_string()),
            kind: RowKind::Sub { depth, show_location: child.cwd != parent.cwd },
            parent: Some((pid, parent_id.to_string())),
            hidden_children: 0,
        });
        push_children(out, pid, child, id, depth + 1, children);
    }
}

/// Garde le curseur sur l'élément sélectionné ; s'il a disparu, passe au plus proche ancêtre
/// encore affiché, sinon à la ligne voisine.
pub fn select_after_rebuild(m: &mut Model, before: &[Row], after: &[Row]) {
    let Some(selected) = m.selected.clone() else {
        m.selected = after.first().map(|r| r.key.clone());
        return;
    };
    let present = |key: &Key| after.iter().any(|r| &r.key == key);
    if present(&selected) {
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
        assert_eq!(rows[2].kind, RowKind::Sub { depth: 2, show_location: false });
        assert_eq!(rows[3].kind, RowKind::Sub { depth: 1, show_location: false });
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
}
