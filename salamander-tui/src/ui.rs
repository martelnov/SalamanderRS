//! Ratatui renderer. No GTK, ncurses, X11 or Wayland linkage.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use salamander_core::model::{self, EntryKind, FileEntry};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::panel::Panel;
use crate::App;

fn clean(text: &str) -> String {
    text.chars().map(|ch| if ch.is_control() { ' ' } else { ch }).collect()
}

/// Truncate to terminal CELL width, not UTF-8 byte length (important for Czech names).
fn fit(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let safe = clean(text);
    let len = UnicodeWidthStr::width(safe.as_str());
    if len <= width {
        return format!("{}{}", safe, " ".repeat(width - len));
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in safe.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w >= width { break; }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

fn file_row(entry: &FileEntry, width: usize, marked: bool) -> String {
    if width == 0 { return String::new(); }
    let marker = if marked { '*' } else { ' ' };
    let symbol = match entry.kind {
        EntryKind::Parent => '↖',
        EntryKind::Directory => '/',
        EntryKind::Symlink => '@',
        EntryKind::File => ' ',
        EntryKind::Other => '?',
    };
    let prefix = format!("{marker}{symbol} ");
    let size = if entry.is_dir_like() || entry.is_parent() {
        "<DIR>".to_owned()
    } else {
        model::format_bytes(entry.size)
    };
    let rights = if width >= 64 { entry.mode_text() } else { String::new() };
    let size_width = if width >= 28 { 11 } else { 0 };
    let rights_width = if width >= 64 { 11 } else { 0 };
    let name_width = width.saturating_sub(UnicodeWidthStr::width(prefix.as_str()) + size_width + rights_width);
    let name = entry.name.to_string_lossy();
    let mut row = format!("{}{}", prefix, fit(&name, name_width));
    if size_width > 0 {
        row.push_str(&fit(&size, size_width));
    }
    if rights_width > 0 {
        row.push_str(&fit(&rights, rights_width));
    }
    row
}

pub fn visible_rows(terminal_height: u16) -> usize {
    // Outer layout: header 1, panels total h-4, panel border 2, column heading 1, footer 1.
    usize::from(terminal_height.saturating_sub(8)).max(1)
}

pub fn draw(frame: &mut Frame<'_>, app: &App) {
    let size = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(2),
        ])
        .split(size);

    let header = Line::from(vec![
        Span::styled(" Salamander RS ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("  Rust · TUI · SSH / Linux konzole"),
    ]);
    frame.render_widget(Paragraph::new(header), chunks[0]);

    let panel_areas = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);
    render_panel(frame, panel_areas[0], &app.panels[0], app.active == 0);
    render_panel(frame, panel_areas[1], &app.panels[1], app.active == 1);

    let status = clean(&app.status);
    frame.render_widget(
        Paragraph::new(fit(&status, usize::from(chunks[2].width)))
            .style(Style::default().fg(Color::Yellow)),
        chunks[2],
    );
    let keys = ["F1 Help", "F2 Rename", "F3 View", "F4 Edit", "F5 Copy", "F6 Move", "F7 Mkdir", "F8 Delete", "F9 Shell", "F10 Quit"];
    frame.render_widget(
        Paragraph::new(Line::from(keys.iter().map(|label| Span::styled(format!(" {label} "), Style::default().fg(Color::Black).bg(Color::Blue))).collect::<Vec<_>>())),
        chunks[3],
    );
    let bottom = " TAB panel   INS označit   Ctrl+L cesta   Ctrl+K SFTP   Ctrl+H skryté ";
    let y = chunks[3].y.saturating_add(1);
    if y < size.height {
        frame.render_widget(Paragraph::new(fit(bottom, size.width.into())), Rect::new(0, y, size.width, 1));
    }
}

fn render_panel(frame: &mut Frame<'_>, area: Rect, panel: &Panel, focused: bool) {
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let title = fit(&panel.display_path(), area.width.saturating_sub(4).into());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(format!(" {title} "));
    let inside = block.inner(area);
    frame.render_widget(block, area);
    if inside.width == 0 || inside.height < 2 { return; }
    let size_label = if inside.width >= 28 { "Velikost" } else { "" };
    frame.render_widget(
        Paragraph::new(file_row_header(inside.width.into(), size_label))
            .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Rect::new(inside.x, inside.y, inside.width, 1),
    );
    let rows = usize::from(inside.height.saturating_sub(2));
    for offset in 0..rows {
        let index = panel.scroll + offset;
        let Some(entry) = panel.entries.get(index) else { break; };
        let y = inside.y + 1 + offset as u16;
        let marked = panel.marked.contains(&entry.path);
        let mut style = if index == panel.cursor && focused {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else if marked {
            Style::default().fg(Color::Yellow)
        } else if entry.is_dir_like() {
            Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        if entry.kind == EntryKind::Symlink && !focused {
            style = style.add_modifier(Modifier::ITALIC);
        }
        frame.render_widget(
            Paragraph::new(file_row(entry, inside.width.into(), marked)).style(style),
            Rect::new(inside.x, y, inside.width, 1),
        );
    }
    let line = format!(" {} položek | označeno {} ", panel.entries.iter().filter(|item| !item.is_parent()).count(), panel.selected_count());
    frame.render_widget(
        Paragraph::new(fit(&line, inside.width.into())).style(Style::default().fg(Color::DarkGray)),
        Rect::new(inside.x, inside.y + inside.height - 1, inside.width, 1),
    );
}

fn file_row_header(width: usize, size_label: &str) -> String {
    let right = if width >= 64 { "Práva" } else { "" };
    let size_width = if width >= 28 { 11 } else { 0 };
    let rights_width = if width >= 64 { 11 } else { 0 };
    let name_width = width.saturating_sub(size_width + rights_width);
    format!("{}{}{}", fit(" Název", name_width), fit(size_label, size_width), fit(right, rights_width))
}

pub fn centered(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

pub fn overlay(frame: &mut Frame<'_>, title: &str, lines: &[String], footer: &str) {
    let max = lines.iter().map(|line| UnicodeWidthStr::width(line.as_str())).max().unwrap_or(30);
    let rect = centered(((max + 6) as u16).clamp(36, 86), (lines.len() as u16).saturating_add(5).max(7), frame.area());
    frame.render_widget(Clear, rect);
    let inner = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", clean(title)))
        .border_style(Style::default().fg(Color::Yellow));
    let area = inner.inner(rect);
    frame.render_widget(inner, rect);
    if area.height == 0 { return; }
    let mut content = lines.iter().map(|line| clean(line)).collect::<Vec<_>>();
    content.push(String::new());
    content.push(clean(footer));
    frame.render_widget(
        Paragraph::new(content.join("\n")).wrap(Wrap { trim: false }),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn width_not_bytes() {
        assert_eq!(fit("český", 6), "český ");
        assert_eq!(fit("třeba", 3), "tř…");
        assert_eq!(fit("xx", 0), "");
    }
}
