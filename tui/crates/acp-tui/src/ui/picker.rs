use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState};

use super::permission_modal::centered;
use crate::app::App;

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let Some(picker) = &app.picker else { return };
    let current = app
        .config(&picker.config_id)
        .map(|o| o.current_value.as_str());

    let items: Vec<ListItem> = picker
        .choices
        .iter()
        .map(|choice| {
            let marker = if Some(choice.value.as_str()) == current {
                "● "
            } else {
                "  "
            };
            ListItem::new(format!("{marker}{}", choice.name))
        })
        .collect();

    let widest = picker
        .choices
        .iter()
        .map(|c| c.name.chars().count())
        .max()
        .unwrap_or(10) as u16;
    let rect = centered(
        area,
        (widest + 8).max(30),
        (items.len() as u16 + 2).min(area.height),
    );
    let mut state = ListState::default().with_selected(Some(picker.selected));

    frame.render_widget(Clear, rect);
    frame.render_stateful_widget(
        List::new(items)
            .block(
                Block::bordered()
                    .title(format!(" {} — ↑/↓ Enter · Esc ", picker.title))
                    .border_style(Style::new().fg(Color::Cyan)),
            )
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
        rect,
        &mut state,
    );
}
