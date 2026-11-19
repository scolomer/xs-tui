//! Rendu : arbre des instances, jauges de consommation, copie de session et barre du bas
//! (SPEC.md § Affichage).

use std::borrow::Cow;
use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{HighlightSpacing, List, ListItem, ListState, Paragraph};
use unicode_width::UnicodeWidthChar;

use crate::capy;
use crate::conversation::{Conversation, Source};
use crate::filter::Query;
use crate::mirror::{Mirror, View};
use crate::model::{self, Model, ROOT_ID, Row, RowKind};
use crate::prompt::Prompt;
use crate::record::State;
use crate::usage::{self, Status};
use crate::zellij;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const HELP: &str =
    "↑↓/jk naviguer  ⏎ aller au pane  f filtrer  w prompt  s reprendre en fond  D passer en fond  ␣ plier  p épingler  x fermer  d détail  q quitter";
const EMPTY: &str = "Aucune instance omp";
const NO_MATCH: &str = "Aucune session ne correspond au filtre";
/// Marque des lignes omp (oh-my-pi).
const OMP_MARK: &str = "π";
/// Marque des threads Capy : `nf-fa-cloud`.
const CAPY_MARK: &str = "\u{f0c2}";
/// Cellules d'une barre de jauge.
const GAUGE_CELLS: usize = 10;
/// Colonnes entre la ligne la plus large de la liste et les jauges.
const GAUGE_GAP: u16 = 3;

pub fn draw(
    frame: &mut Frame,
    m: &Model,
    rows: &[Row],
    own_zellij: Option<&str>,
    list: &mut ListState,
    mirror: Option<&Mirror>,
    prompt: Option<&Prompt>,
) {
    let area = frame.area();
    let [top, bar_area] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
    // La ligne du filtre, en saisie ou gardé, suit la dernière ligne de la liste.
    let filter_line = u16::from(m.filtering || !m.filter.trim().is_empty());
    let (list_area, copy) = match mirror {
        None => (top, None),
        Some(mirror) => {
            // La liste prend la hauteur de ses lignes, au plus un tiers de l'écran ; elle défile au-delà.
            // Une ligne vide la sépare du trait.
            let cap = usize::from((area.height.saturating_sub(1) / 3).max(1));
            let height = rows.len().clamp(1, cap) as u16 + filter_line;
            let [list_area, _, separator_area, copy_area] = Layout::vertical([
                Constraint::Length(height),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .areas(top);
            (list_area, Some((mirror, separator_area, copy_area)))
        }
    };

    // Les titres des racines, des sessions dormantes et des threads Capy commencent tous à la même
    // colonne, après le plus large dossier/branche ou projet.
    let title_col = rows
        .iter()
        .filter_map(|r| match r.kind {
            // La largeur du segment ne dépend pas de son style.
            RowKind::Root => m.instances.get(&r.key).map(|rec| width(&location(m, &rec.cwd, Folder::Live, None))),
            RowKind::Dormant => m.dormant(&r.key).map(|p| width(&location(m, &p.cwd, Folder::Live, None))),
            RowKind::Capy => m.capy_thread(&r.key).map(|t| width(&project_segment(&t.project, None))),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let query = Query::new(&m.filter);
    let lines: Vec<Line> = rows.iter().map(|row| row_line(m, row, own_zellij, title_col, query.as_ref())).collect();
    // `› ` devant chaque ligne.
    let empty = if m.filter.trim().is_empty() { EMPTY } else { NO_MATCH };
    let list_width = lines.iter().map(|l| l.width() + 2).max().unwrap_or(empty.chars().count());
    let (list_area, usage_hidden) = draw_usage(frame, m, list_area, list_width);
    let list_area = if filter_line == 0 {
        list_area
    } else {
        let height = (rows.len().max(1) as u16).min(list_area.height.saturating_sub(1));
        let [list_area, filter_area, _] =
            Layout::vertical([Constraint::Length(height), Constraint::Length(1), Constraint::Min(0)]).areas(list_area);
        draw_input(frame, "Filtre › ".to_string(), &m.filter, filter_area, m.filtering);
        list_area
    };

    if rows.is_empty() {
        centered(frame, empty, list_area);
    } else {
        list.select(m.selected.as_ref().and_then(|key| rows.iter().position(|r| &r.key == key)));
        let widget = List::new(lines.into_iter().map(ListItem::new))
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::new().add_modifier(Modifier::BOLD));
        frame.render_stateful_widget(widget, list_area, list);
    }
    if let Some((mirror, separator_area, copy_area)) = copy {
        draw_copy(frame, m, rows, mirror, separator_area, copy_area);
    }
    if let Some(prompt) = prompt {
        let head =
            if prompt.title.is_empty() { "Prompt › ".to_string() } else { format!("Prompt → “{}” › ", prompt.title) };
        draw_input(frame, head, &prompt.text, bar_area, true);
    } else {
        draw_bar(frame, m, rows, bar_area, usage_hidden);
    }
}

/// Jauges de consommation dans une colonne à droite de `area`, si la liste (`list_width`) garde sa
/// largeur ; les jauges qui dépassent en hauteur sont remplacées par `+N jauges masquées`. Renvoie
/// la zone laissée à la liste, et vrai si la colonne entière est masquée faute de largeur.
fn draw_usage(frame: &mut Frame, m: &Model, area: Rect, list_width: usize) -> (Rect, bool) {
    let mut lines = usage_lines(m, usage::now_ms());
    let total = lines.iter().filter(|(_, gauge)| *gauge).count();
    if lines.len() > usize::from(area.height) {
        // La dernière ligne disponible signale les jauges omises.
        lines.truncate(usize::from(area.height.saturating_sub(1)));
        // Pas d'en-tête de compte ni de ligne vide sans jauge en dessous.
        while lines.last().is_some_and(|(_, gauge)| !gauge) {
            lines.pop();
        }
        let hidden = total - lines.iter().filter(|(_, gauge)| *gauge).count();
        let s = if hidden > 1 { "s" } else { "" };
        lines.push((Line::raw(format!("+{hidden} jauge{s} masquée{s}")).dim().right_aligned(), false));
    }
    let panel_width = lines.iter().map(|(l, _)| l.width()).max().unwrap_or(0) as u16;
    if panel_width == 0 || area.height == 0 {
        return (area, false);
    }
    if usize::from(area.width) < list_width + usize::from(GAUGE_GAP + panel_width) {
        return (area, true);
    }
    let [list_area, _, panel_area] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(GAUGE_GAP), Constraint::Length(panel_width)])
            .areas(area);
    frame.render_widget(Paragraph::new(lines.into_iter().map(|(l, _)| l).collect::<Vec<_>>()), panel_area);
    (list_area, false)
}

/// Lignes du panneau, chacune marquée vraie si c'est une jauge : par compte, son nom puis une jauge
/// par fenêtre, `7 j  ██░░░░░░░░  22 %  1j14h`. Sans aucune jauge, le dernier échec de relecture.
fn usage_lines(m: &Model, now_ms: u64) -> Vec<(Line<'_>, bool)> {
    if m.usage.is_empty() {
        let error = |e: &String| Line::raw(e.chars().take(40).collect::<String>()).dim();
        return m.usage_error.iter().map(|e| (error(e), true)).collect();
    }
    let label_width = m.usage.iter().flat_map(|a| &a.gauges).map(|g| g.label.chars().count()).max().unwrap_or(0);
    let mut out = Vec::new();
    for account in &m.usage {
        if !out.is_empty() {
            out.push((Line::default(), false));
        }
        out.push((Line::raw(account.name.as_str()).bold(), false));
        for g in &account.gauges {
            let filled = ((g.used * GAUGE_CELLS as f64).round() as usize).min(GAUGE_CELLS);
            let color = match g.status {
                Status::Ok => Color::Green,
                Status::Warning => Color::Yellow,
                Status::Exhausted => Color::Red,
                Status::Unknown => Color::DarkGray,
            };
            let reset = g.resets_at.filter(|&at| at > now_ms).map_or_else(String::new, |at| countdown(at - now_ms));
            let line = Line::from(vec![
                Span::raw(format!("{:<label_width$}  ", g.label)),
                Span::styled("█".repeat(filled), color),
                Span::styled("░".repeat(GAUGE_CELLS - filled), Color::DarkGray),
                Span::raw(format!(" {:>3} %", (g.used * 100.0).round() as u32)),
                Span::styled(format!("  {reset:>5}"), Color::DarkGray),
            ]);
            out.push((line, true));
        }
    }
    out
}

/// Délai avant un reset : `45m`, `3h02`, `1j14h`.
fn countdown(ms: u64) -> String {
    let minutes = ms.div_ceil(60_000);
    let (days, hours, mins) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours) {
        (0, 0) => format!("{mins}m"),
        (0, _) => format!("{hours}h{mins:02}"),
        _ => format!("{days}j{hours}h"),
    }
}

/// Ligne de séparation puis écran visible du pane copié, coupé en haut et à droite, ou conversation
/// du thread Capy.
fn draw_copy(frame: &mut Frame, m: &Model, rows: &[Row], mirror: &Mirror, separator_area: Rect, area: Rect) {
    let row = m.selected.as_ref().and_then(|k| rows.iter().find(|r| &r.key == k));
    let crop = match &mirror.view {
        View::Screen(text) if text.width() > usize::from(area.width) => {
            Some(format!(" {}×{} → {}×{} ", text.width(), text.lines.len(), area.width, area.height))
        }
        _ => None,
    };
    let line = separator(model::copy_title(m, row), crop.as_deref(), separator_area.width);
    frame.render_widget(Paragraph::new(line), separator_area);
    match &mirror.view {
        View::Waiting => {}
        View::Message(message) => centered(frame, message, area),
        View::Conversation(conversation) => draw_conversation(frame, conversation, m.tick, area),
        View::Screen(text) => {
            // omp écrit de haut en bas et termine par son éditeur et son statut, sans forcément
            // remplir l'écran : on garde les dernières lignes non vides.
            let used = text
                .lines
                .iter()
                .rposition(|l| l.spans.iter().any(|s| !s.content.trim().is_empty()))
                .map_or(0, |i| i + 1);
            let skip = used.saturating_sub(usize::from(area.height));
            let buf = frame.buffer_mut();
            for (y, line) in (area.y..area.bottom()).zip(text.lines.iter().skip(skip)) {
                buf.set_line(area.x, y, line, area.width);
            }
        }
    }
}

/// Fin de la conversation d'un thread Capy, retours à la ligne à la largeur de `area` : messages de
/// l'utilisateur en gras derrière `›`, réponses en texte normal, outils sur une ligne grise, puis le
/// tour en cours (réponse en écriture, outil appelé).
fn draw_conversation(frame: &mut Frame, c: &Conversation, tick: usize, area: Rect) {
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    let tool_line = |prefix: Span<'static>, tool: &str, text: &str| {
        let first = text.lines().next().unwrap_or_default();
        let summary = if first.is_empty() { tool.to_string() } else { format!("{tool} : {first}") };
        vec![Line::from(vec![prefix, Span::raw(summary).fg(Color::DarkGray)])]
    };
    let block = |source: Source, text: &str| -> Vec<Line<'static>> {
        let mut lines = vec![Line::default()];
        match source {
            Source::User => {
                let style = Style::new().add_modifier(Modifier::BOLD);
                for (i, l) in wrap(text, width.saturating_sub(2)).into_iter().enumerate() {
                    let head = if i == 0 { Span::styled("› ", Color::Cyan) } else { Span::raw("  ") };
                    lines.push(Line::from(vec![head, Span::styled(l, style)]));
                }
            }
            _ => lines.extend(wrap(text, width).into_iter().map(Line::raw)),
        }
        lines
    };
    // Blocs du plus récent au plus ancien, jusqu'à remplir la zone.
    let mut blocks: Vec<Vec<Line>> = Vec::new();
    if let Some((_, name, text)) = &c.tool_call {
        blocks.push(tool_line(Span::styled(format!("{} ", SPINNER[tick % SPINNER.len()]), Color::Yellow), name, text));
    }
    blocks.extend(c.live.iter().rev().map(|(_, text)| block(Source::Assistant, text)));
    let mut used: usize = blocks.iter().map(Vec::len).sum();
    for entry in c.entries.iter().rev() {
        if used >= height {
            break;
        }
        let lines = match entry.source {
            Source::Tool => tool_line(Span::raw("⏵ ").fg(Color::DarkGray), entry.tool.as_deref().unwrap_or("outil"), &entry.text),
            source => block(source, &entry.text),
        };
        used += lines.len();
        blocks.push(lines);
    }
    let lines: Vec<Line> = blocks.into_iter().rev().flatten().collect();
    let skip = lines.len().saturating_sub(height);
    let buf = frame.buffer_mut();
    for (y, line) in (area.y..area.bottom()).zip(lines.iter().skip(skip)) {
        buf.set_line(area.x, y, line, area.width);
    }
}

/// `text` coupé en lignes de `width` colonnes au plus, aux espaces quand c'est possible ; les
/// tabulations deviennent des espaces et les autres caractères de contrôle disparaissent.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for raw in text.lines() {
        let mut line = String::new();
        let mut used = 0;
        for word in raw.split(' ') {
            let word: String = word.chars().filter_map(|c| if c == '\t' { Some(' ') } else { (!c.is_control()).then_some(c) }).collect();
            let w: usize = word.chars().map(|c| c.width().unwrap_or(0)).sum();
            if used > 0 && used + 1 + w <= width {
                line.push(' ');
                line.push_str(&word);
                used += 1 + w;
                continue;
            }
            if used > 0 {
                out.push(std::mem::take(&mut line));
                used = 0;
            }
            // Un mot plus large que la ligne est coupé n'importe où.
            for c in word.chars() {
                let cw = c.width().unwrap_or(0);
                if used + cw > width {
                    out.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(c);
                used += cw;
            }
        }
        out.push(line);
    }
    out
}

/// `── <titre> ──────── <taille source → zone> ──`, la taille seulement si la largeur est coupée.
fn separator<'a>(title: Option<&'a str>, crop: Option<&str>, width: u16) -> Line<'a> {
    let gray = Style::new().fg(Color::DarkGray);
    let mut spans = vec![Span::styled("── ", gray)];
    if let Some(title) = title.filter(|t| !t.is_empty()) {
        spans.push(Span::raw(title));
        spans.push(Span::raw(" "));
    }
    let right = crop.map(|c| [Span::styled(c.to_string(), gray), Span::styled("──", gray)]);
    let used = self::width(&spans) + right.as_ref().map_or(0, |r| self::width(r));
    spans.push(Span::styled("─".repeat(usize::from(width).saturating_sub(used)), gray));
    spans.extend(right.into_iter().flatten());
    Line::from(spans)
}

/// Message grisé centré dans `area`.
fn centered(frame: &mut Frame, message: &str, area: Rect) {
    let middle = Rect { y: area.y + area.height / 2, height: 1.min(area.height), ..area };
    frame.render_widget(Paragraph::new(message).dim().alignment(Alignment::Center), middle);
}

fn indicator(state: State, tick: usize) -> Span<'static> {
    match state {
        State::Working => Span::styled(SPINNER[tick % SPINNER.len()].to_string(), Color::Yellow),
        State::Idle => Span::styled("●", Color::Green),
        State::Blocked => Span::styled("●", Color::Magenta),
        State::Error => Span::styled("●", Color::Red),
    }
}

/// Thread Capy `waiting` (CI, sous-agent, minuteur) : spinner gris, trois fois plus lent.
fn waiting_indicator(tick: usize) -> Span<'static> {
    Span::styled(SPINNER[tick / 3 % SPINNER.len()].to_string(), Color::DarkGray)
}

/// État omp équivalent d'un thread Capy ; `None` pour `waiting`, qui n'en a pas.
fn capy_state(t: &capy::Thread) -> Option<State> {
    if t.needs_you {
        return Some(State::Blocked);
    }
    match t.status {
        capy::Status::Working => Some(State::Working),
        capy::Status::Waiting => None,
        capy::Status::Failed => Some(State::Error),
        capy::Status::Idle | capy::Status::Archived => Some(State::Idle),
    }
}

/// Marque de la source d'une ligne de premier niveau : π pour omp, nuage pour Capy.
fn source_mark(capy: bool) -> Span<'static> {
    if capy { Span::styled(CAPY_MARK, Color::Cyan) } else { Span::styled(OMP_MARK, Color::Blue) }
}

/// Segment powerline du projet d'un thread Capy, de la largeur de son texte.
fn project_segment<'a>(project: &'a str, query: Option<&Query>) -> Vec<Span<'a>> {
    let style = Style::new().bg(Color::Cyan).fg(Color::Black);
    let mut out = vec![Span::styled(" ", style)];
    out.extend(marked(project.into(), style, query));
    out.extend([Span::styled(" ", style), Span::styled("\u{e0b0}", Color::Cyan), Span::raw(" ")]);
    out
}

/// `<état> <nuage> <projet> <titre>`, le titre en gras s'il a une activité non lue.
fn capy_line<'a>(m: &Model, t: &'a capy::Thread, title_col: usize, query: Option<&Query>) -> Line<'a> {
    let state = match capy_state(t) {
        Some(state) => indicator(state, m.tick),
        None => waiting_indicator(m.tick),
    };
    let mut spans = vec![state, Span::raw(" "), source_mark(true), Span::raw(" ")];
    let segment = project_segment(&t.project, query);
    let pad = (title_col + 1).saturating_sub(width(&segment));
    spans.extend(segment);
    spans.push(Span::raw(" ".repeat(pad)));
    let title = if t.unread { Style::new().add_modifier(Modifier::BOLD) } else { Style::new() };
    spans.extend(marked(t.title.as_str().into(), title, query));
    Line::from(spans)
}

fn row_line<'a>(m: &'a Model, row: &Row, own_zellij: Option<&str>, title_col: usize, query: Option<&Query>) -> Line<'a> {
    let pid = row.key.0;
    if row.kind == RowKind::Capy {
        return m.capy_thread(&row.key).map_or_else(Line::default, |t| capy_line(m, t, title_col, query));
    }
    if row.kind == RowKind::Dormant {
        let Some(p) = m.dormant(&row.key) else { return Line::default() };
        let mut spans = vec![Span::styled("○", Color::DarkGray), Span::raw(" "), source_mark(false), Span::raw(" ")];
        let location = location(m, &p.cwd, Folder::Dormant, query);
        let pad = (title_col + 1).saturating_sub(width(&location));
        spans.extend(location);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(marked(p.title.as_str().into(), Style::new(), query));
        spans.push(pin());
        return Line::from(spans);
    }
    let Some(rec) = m.instances.get(&row.key).filter(|_| row.kind != RowKind::Error) else {
        let message = m.errors.get(&pid).map_or("", String::as_str);
        let mut spans = vec![Span::styled("⚠ ", Color::Red)];
        spans.extend(marked(format!("pid {pid} : {message}").into(), Style::new(), query));
        return Line::from(spans);
    };
    let root = m.instances.get(&(pid, ROOT_ID.to_string())).unwrap_or(rec);
    let dim = !zellij::reachable(own_zellij, root.zellij.as_ref());

    // Spans hors indicateur : atténués si l'instance est hors de portée.
    let mut before = Vec::new();
    let mut after = Vec::new();
    // Un sous-agent n'existe que pendant son travail : son indicateur ne sert qu'à signaler une
    // attente de l'utilisateur.
    let (show_location, show_indicator) = match row.kind {
        RowKind::Root => (true, true),
        RowKind::Sub { show_location } => {
            before.push(Span::raw(row.tree.clone()));
            (show_location, rec.state == State::Blocked)
        }
        RowKind::Error | RowKind::Dormant | RowKind::Capy => unreachable!(),
    };
    if show_location {
        let style = if m.is_headless(own_zellij, root.zellij.as_ref()) { Folder::Headless } else { Folder::Live };
        after = location(m, &rec.cwd, style, query);
    }
    match row.kind {
        RowKind::Root => {
            // Un espace de plus que la plus large branche, pour bien séparer le titre.
            let pad = (title_col + 1).saturating_sub(width(&after));
            after.push(Span::raw(" ".repeat(pad)));
            after.extend(marked(rec.title.as_str().into(), Style::new(), query));
            if row.hidden_children > 0 {
                after.push(Span::raw(format!(" ▸ {}", row.hidden_children)).dim());
            }
            if m.is_pinned(&rec.session_id) {
                after.push(pin());
            }
        }
        _ => {
            after.extend(marked(rec.agent.id.as_str().into(), Style::new(), query));
            after.push(Span::raw(" "));
            after.extend(marked(rec.agent.name.as_str().into(), Style::new().fg(Color::DarkGray), query));
        }
    }

    let dimmed = |spans: Vec<Span<'a>>| spans.into_iter().map(move |s| if dim { s.dim() } else { s });
    let mut spans: Vec<Span> = dimmed(before).collect();
    if show_indicator {
        spans.push(indicator(rec.state, m.tick));
        spans.extend(dimmed(vec![Span::raw(" ")]));
    }
    if row.kind == RowKind::Root {
        spans.extend(dimmed(vec![source_mark(false)]));
        spans.extend(dimmed(vec![Span::raw(" ")]));
    }
    spans.extend(dimmed(after));
    Line::from(spans)
}

/// Épingle `nf-md-pin` grise en fin de ligne d'une session épinglée.
fn pin() -> Span<'static> {
    Span::styled(" \u{f0403}", Color::DarkGray)
}

/// `text` dans `style`, les lettres trouvées par le filtre en gras souligné : la couleur reste, le
/// surlignage se voit aussi sur le fond du segment dossier.
fn marked<'a>(text: Cow<'a, str>, style: Style, query: Option<&Query>) -> Vec<Span<'a>> {
    let hits = query.map(|q| q.highlight(&text)).filter(|hits| hits.contains(&true));
    let Some(hits) = hits else { return vec![Span::styled(text, style)] };
    let hit_style = style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_hit = false;
    for (c, hit) in text.chars().zip(hits) {
        if hit != run_hit && !run.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut run), if run_hit { hit_style } else { style }));
        }
        run_hit = hit;
        run.push(c);
    }
    spans.push(Span::styled(run, if run_hit { hit_style } else { style }));
    spans
}

/// Style du segment dossier : fond bleu pour une instance, gris foncé pour une instance headless,
/// aucun fond pour une session dormante (dossier gris, espace à la place de la flèche).
#[derive(Clone, Copy)]
enum Folder {
    Live,
    Headless,
    Dormant,
}

/// Segment powerline du dossier, puis branche git s'il y en a une, comme le segment git d'omp :
/// branche verte si le dépôt est propre, jaune sinon, suivie des compteurs `*modifiés`,
/// `+indexés` et `?non suivis` non nuls. La largeur ne dépend ni de `style` ni de `query`.
fn location<'a>(m: &'a Model, cwd: &Path, style: Folder, query: Option<&Query>) -> Vec<Span<'a>> {
    let (segment, arrow) = match style {
        Folder::Live => (Style::new().bg(Color::Blue).fg(Color::Black), Span::styled("\u{e0b0}", Color::Blue)),
        Folder::Headless => {
            (Style::new().bg(Color::DarkGray).fg(Color::Black), Span::styled("\u{e0b0}", Color::DarkGray))
        }
        Folder::Dormant => (Style::new().fg(Color::DarkGray), Span::raw(" ")),
    };
    let mut out = vec![Span::styled(" \u{f115} ", segment)];
    out.extend(marked(m.folder_label(cwd).into(), segment, query));
    out.extend([Span::styled(" ", segment), arrow, Span::raw(" ")]);
    if let Some(repo) = m.repo(cwd)
        && let Some(branch) = &repo.label
    {
        let c = repo.counts;
        let color = Style::new().fg(if c.dirty() { Color::Yellow } else { Color::Green });
        out.push(Span::styled("\u{f126} ", color));
        out.extend(marked(branch.as_str().into(), color, query));
        for (n, sign, color) in
            [(c.unstaged, '*', Color::Yellow), (c.staged, '+', Color::Green), (c.untracked, '?', Color::DarkGray)]
        {
            if n > 0 {
                out.push(Span::styled(format!(" {sign}{n}"), color));
            }
        }
        out.push(Span::raw(" "));
    }
    out
}

fn width(spans: &[Span]) -> usize {
    spans.iter().map(Span::width).sum()
}

/// Barre du bas ; `usage_hidden` signale que les jauges n'ont pas la place de s'afficher.
fn draw_bar(frame: &mut Frame, m: &Model, rows: &[Row], area: Rect, usage_hidden: bool) {
    if let Some((message, _)) = &m.message {
        frame.render_widget(Paragraph::new(message.as_str()).fg(Color::Yellow), area);
        return;
    }
    // Un thread Capy compte dans l'état omp équivalent ; `waiting` a son propre compteur.
    let mut counts = [0usize; 4];
    let mut waiting = 0;
    for row in rows {
        if let Some(t) = m.capy_thread(&row.key) {
            match capy_state(t) {
                Some(state) => counts[state as usize] += 1,
                None => waiting += 1,
            }
        } else if let Some(rec) = m.instances.get(&row.key).filter(|_| row.kind != RowKind::Error) {
            counts[rec.state as usize] += 1;
        }
    }
    let mut spans = Vec::new();
    let push = |spans: &mut Vec<Span>, span: Span<'static>| {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(span);
    };
    for state in [State::Working, State::Idle, State::Blocked, State::Error] {
        let n = counts[state as usize];
        if n > 0 {
            let glyph = indicator(state, m.tick);
            push(&mut spans, Span::styled(format!("{n} {}", glyph.content), glyph.style));
        }
        if state == State::Working && waiting > 0 {
            let glyph = waiting_indicator(m.tick);
            push(&mut spans, Span::styled(format!("{waiting} {}", glyph.content), glyph.style));
        }
    }
    if let Some(error) = &m.capy_error {
        push(&mut spans, Span::styled(format!("capy : {error}"), Color::Red));
    }
    if let Some(status) = &m.manager {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(match &status.problem {
            None => Span::raw(format!("manager : {} parleys", status.parleys)).dim(),
            Some(problem) => Span::styled(format!("manager : {problem}"), Color::Red),
        });
    }
    if usage_hidden {
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::raw("jauges masquées").dim());
    }
    let left = Line::from(spans);
    let help_width = Line::raw(HELP).width() as u16;
    if left.width() as u16 + 2 + help_width <= area.width {
        let [left_area, help_area] =
            Layout::horizontal([Constraint::Min(0), Constraint::Length(help_width)]).areas(area);
        frame.render_widget(Paragraph::new(left), left_area);
        frame.render_widget(Paragraph::new(HELP).dim(), help_area);
    } else {
        frame.render_widget(Paragraph::new(left), area);
    }
}

/// Ligne de saisie (prompt `w` dans la barre, filtre `f` sous la liste) : `head` puis le texte. Le
/// début d'un texte trop long est coupé ; un retour à la ligne s'affiche `↵`. En saisie, `head` est
/// en gras et le curseur en fin de texte ; sinon (filtre gardé), `head` est atténué, sans curseur.
fn draw_input(frame: &mut Frame, head: String, text: &str, area: Rect, focused: bool) {
    let style = Style::new().fg(Color::Cyan);
    let head = Span::styled(head, if focused { style.add_modifier(Modifier::BOLD) } else { style.dim() });
    // Une colonne reste pour le curseur.
    let room = usize::from(area.width).saturating_sub(head.width() + 1);
    let mut used = 0;
    let mut start = text.len();
    for (i, c) in text.char_indices().rev() {
        let w = shown(c).width().unwrap_or(0);
        if used + w > room {
            break;
        }
        used += w;
        start = i;
    }
    let cursor = area.x.saturating_add((head.width() + used) as u16).min(area.right().saturating_sub(1));
    let text: String = text[start..].chars().map(shown).collect();
    frame.render_widget(Paragraph::new(Line::from(vec![head, Span::raw(text)])), area);
    if focused {
        frame.set_cursor_position((cursor, area.y));
    }
}

/// Caractère affiché pour `c` dans la saisie : `↵` pour un retour à la ligne, une espace pour un
/// autre caractère de contrôle (tabulation collée).
fn shown(c: char) -> char {
    match c {
        '\n' => '↵',
        c if c.is_control() => ' ',
        c => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_at_spaces_and_splits_words_wider_than_the_line() {
        assert_eq!(wrap("Tests pass. PR is up.", 11), ["Tests pass.", "PR is up."]);
        assert_eq!(wrap("abcdefgh ij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("a\n\nb\tc", 10), ["a", "", "b c"]);
    }
}
