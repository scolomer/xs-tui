//! Rendu : arbre des instances et barre du bas (SPEC.md § Affichage).

use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{HighlightSpacing, List, ListItem, ListState, Paragraph};

use crate::model::{Instance, Model, ROOT_ID, Row, RowKind};
use crate::record::State;
use crate::zellij;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const HELP: &str = "↑↓/jk naviguer  ⏎ aller au pane  ␣ plier  q quitter";

pub fn draw(frame: &mut Frame, m: &Model, rows: &[Row], own_zellij: Option<&str>, list: &mut ListState) {
    let [list_area, bar_area] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

    if rows.is_empty() {
        let middle = Rect { y: list_area.y + list_area.height / 2, height: 1.min(list_area.height), ..list_area };
        frame.render_widget(Paragraph::new("Aucune instance omp").dim().alignment(Alignment::Center), middle);
    } else {
        // Les titres des racines commencent tous à la même colonne, après le plus large dossier/branche.
        let title_col = rows
            .iter()
            .filter(|r| r.kind == RowKind::Root)
            .filter_map(|r| m.instances.get(&r.key))
            .map(|inst| width(&location(inst)))
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

    draw_bar(frame, m, rows, bar_area);
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
    let Some(inst) = m.instances.get(&row.key).filter(|_| row.kind != RowKind::Error) else {
        let message = m.errors.get(&pid).map_or("", String::as_str);
        return Line::from(vec![Span::styled("⚠ ", Color::Red), Span::raw(format!("pid {pid} : {message}"))]);
    };
    let root = m.instances.get(&(pid, ROOT_ID.to_string())).unwrap_or(inst);
    let dim = !zellij::reachable(own_zellij, root.rec.zellij.as_ref());

    // Spans hors indicateur : atténués si l'instance est hors de portée.
    let mut before = Vec::new();
    let mut after = Vec::new();
    let show_location = match row.kind {
        RowKind::Root => true,
        RowKind::Sub { depth, show_location } => {
            before.push(Span::raw(format!("{}├ ", "  ".repeat(depth as usize))));
            show_location
        }
        RowKind::Error => unreachable!(),
    };
    if show_location {
        after = location(inst);
    }
    match row.kind {
        RowKind::Root => {
            let pad = title_col.saturating_sub(width(&after));
            if pad > 0 {
                after.push(Span::raw(" ".repeat(pad)));
            }
            after.push(Span::raw(inst.rec.title.as_str()));
            if row.hidden_children > 0 {
                after.push(Span::raw(format!(" ▸ {}", row.hidden_children)).dim());
            }
        }
        _ => {
            after.push(Span::raw(inst.rec.agent.id.as_str()));
            after.push(Span::raw(" "));
            after.push(Span::styled(inst.rec.agent.name.as_str(), Color::DarkGray));
        }
    }

    let dimmed = |spans: Vec<Span<'a>>| spans.into_iter().map(move |s| if dim { s.dim() } else { s });
    let mut spans: Vec<Span> = dimmed(before).collect();
    spans.push(indicator(inst.rec.state, m.tick));
    spans.extend(dimmed(vec![Span::raw(" ")]));
    spans.extend(dimmed(after));
    Line::from(spans)
}

/// Segment powerline du dossier, puis branche git s'il y en a une.
fn location(inst: &Instance) -> Vec<Span<'_>> {
    let folder = folder_name(&inst.rec.cwd);
    let mut out = vec![
        Span::styled(format!(" \u{f115} {folder} "), Style::new().bg(Color::Blue).fg(Color::Black)),
        Span::styled("\u{e0b0}", Color::Blue),
        Span::raw(" "),
    ];
    if let Some(branch) = &inst.branch {
        out.push(Span::raw(format!("\u{f126} {branch} ")));
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
        if let Some(inst) = m.instances.get(&row.key).filter(|_| row.kind != RowKind::Error) {
            counts[inst.rec.state as usize] += 1;
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
