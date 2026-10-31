//! Rendu : arbre des instances, copie de session et barre du bas (SPEC.md § Affichage).

use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{HighlightSpacing, List, ListItem, ListState, Paragraph};

use crate::mirror::{Mirror, View};
use crate::model::{self, Model, ROOT_ID, Row, RowKind};
use crate::record::State;
use crate::zellij;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const HELP: &str = "↑↓/jk naviguer  ⏎ aller au pane  ␣ plier  d détail  q quitter";

pub fn draw(
    frame: &mut Frame,
    m: &Model,
    rows: &[Row],
    own_zellij: Option<&str>,
    list: &mut ListState,
    mirror: Option<&Mirror>,
) {
    let area = frame.area();
    let (list_area, copy, bar_area) = match mirror {
        None => {
            let [list_area, bar_area] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
            (list_area, None, bar_area)
        }
        Some(mirror) => {
            // La liste prend la hauteur de ses lignes, au plus un tiers de l'écran ; elle défile au-delà.
            // Une ligne vide la sépare du trait.
            let cap = usize::from((area.height.saturating_sub(1) / 3).max(1));
            let height = rows.len().clamp(1, cap) as u16;
            let [list_area, _, separator_area, copy_area, bar_area] = Layout::vertical([
                Constraint::Length(height),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(area);
            (list_area, Some((mirror, separator_area, copy_area)), bar_area)
        }
    };

    if rows.is_empty() {
        centered(frame, "Aucune instance omp", list_area);
    } else {
        // Les titres des racines et des sessions dormantes commencent tous à la même colonne,
        // après le plus large dossier/branche.
        let title_col = rows
            .iter()
            .filter_map(|r| match r.kind {
                RowKind::Root => m.instances.get(&r.key).map(|rec| rec.cwd.as_path()),
                RowKind::Dormant => m.dormant(&r.key).map(|p| p.cwd.as_path()),
                _ => None,
            })
            .map(|cwd| width(&location(m, cwd)))
            .max()
            .unwrap_or(0);
        let items: Vec<ListItem> =
            rows.iter().map(|row| ListItem::new(row_line(m, row, own_zellij, title_col))).collect();
        list.select(m.selected.as_ref().and_then(|key| rows.iter().position(|r| &r.key == key)));
        let widget = List::new(items)
            .highlight_symbol("› ")
            .highlight_spacing(HighlightSpacing::Always)
            .highlight_style(Style::new().add_modifier(Modifier::BOLD));
        frame.render_stateful_widget(widget, list_area, list);
    }
    if let Some((mirror, separator_area, copy_area)) = copy {
        draw_copy(frame, m, rows, mirror, separator_area, copy_area);
    }
    draw_bar(frame, m, rows, bar_area);
}

/// Ligne de séparation puis écran visible du pane copié, coupé en haut et à droite.
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

fn row_line<'a>(m: &'a Model, row: &Row, own_zellij: Option<&str>, title_col: usize) -> Line<'a> {
    let pid = row.key.0;
    if row.kind == RowKind::Dormant {
        let Some(p) = m.dormant(&row.key) else { return Line::default() };
        let mut spans = vec![Span::styled("○", Color::DarkGray), Span::raw(" ")];
        spans.extend(location(m, &p.cwd));
        let pad = (title_col + 1).saturating_sub(width(&spans[2..]));
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::raw(p.title.as_str()));
        spans.push(pin());
        return Line::from(spans);
    }
    let Some(rec) = m.instances.get(&row.key).filter(|_| row.kind != RowKind::Error) else {
        let message = m.errors.get(&pid).map_or("", String::as_str);
        return Line::from(vec![Span::styled("⚠ ", Color::Red), Span::raw(format!("pid {pid} : {message}"))]);
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
        RowKind::Error | RowKind::Dormant => unreachable!(),
    };
    if show_location {
        after = location(m, &rec.cwd);
    }
    match row.kind {
        RowKind::Root => {
            // Un espace de plus que la plus large branche, pour bien séparer le titre.
            let pad = (title_col + 1).saturating_sub(width(&after));
            after.push(Span::raw(" ".repeat(pad)));
            after.push(Span::raw(rec.title.as_str()));
            if row.hidden_children > 0 {
                after.push(Span::raw(format!(" ▸ {}", row.hidden_children)).dim());
            }
            if m.is_pinned(&rec.session_id) {
                after.push(pin());
            }
        }
        _ => {
            after.push(Span::raw(rec.agent.id.as_str()));
            after.push(Span::raw(" "));
            after.push(Span::styled(rec.agent.name.as_str(), Color::DarkGray));
        }
    }

    let dimmed = |spans: Vec<Span<'a>>| spans.into_iter().map(move |s| if dim { s.dim() } else { s });
    let mut spans: Vec<Span> = dimmed(before).collect();
    if show_indicator {
        spans.push(indicator(rec.state, m.tick));
        spans.extend(dimmed(vec![Span::raw(" ")]));
    }
    spans.extend(dimmed(after));
    Line::from(spans)
}

/// Épingle `nf-md-pin` grise en fin de ligne d'une session épinglée.
fn pin() -> Span<'static> {
    Span::styled(" \u{f0403}", Color::DarkGray)
}

/// Segment powerline du dossier, puis branche git s'il y en a une, comme le segment git d'omp :
/// branche verte si le dépôt est propre, jaune sinon, suivie des compteurs `*modifiés`,
/// `+indexés` et `?non suivis` non nuls.
fn location<'a>(m: &'a Model, cwd: &Path) -> Vec<Span<'a>> {
    let folder = folder_name(cwd);
    let mut out = vec![
        Span::styled(format!(" \u{f115} {folder} "), Style::new().bg(Color::Blue).fg(Color::Black)),
        Span::styled("\u{e0b0}", Color::Blue),
        Span::raw(" "),
    ];
    if let Some(repo) = m.repo(cwd)
        && let Some(branch) = &repo.label
    {
        let c = repo.counts;
        out.push(Span::styled(format!("\u{f126} {branch}"), if c.dirty() { Color::Yellow } else { Color::Green }));
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

fn folder_name(cwd: &Path) -> String {
    cwd.file_name().unwrap_or(cwd.as_os_str()).to_string_lossy().into_owned()
}

fn draw_bar(frame: &mut Frame, m: &Model, rows: &[Row], area: Rect) {
    if let Some((message, _)) = &m.message {
        frame.render_widget(Paragraph::new(message.as_str()).fg(Color::Yellow), area);
        return;
    }
    let mut counts = [0usize; 4];
    for row in rows {
        if let Some(rec) = m.instances.get(&row.key).filter(|_| row.kind != RowKind::Error) {
            counts[rec.state as usize] += 1;
        }
    }
    let mut spans = Vec::new();
    for state in [State::Working, State::Idle, State::Blocked, State::Error] {
        let n = counts[state as usize];
        if n == 0 {
            continue;
        }
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        let glyph = indicator(state, m.tick);
        spans.push(Span::styled(format!("{n} {}", glyph.content), glyph.style));
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
