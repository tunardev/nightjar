pub mod jobs;
pub mod output;
pub mod runs;

use crate::app::{App, LevelKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    match app.level_kind() {
        LevelKind::Jobs => jobs::render_jobs(frame, chunks[0], app),
        LevelKind::Runs => runs::render_runs(frame, chunks[0], app),
        LevelKind::Output => output::render_output_view(frame, chunks[0], app),
    }
    render_footer(frame, chunks[1], app);
}

fn render_footer(frame: &mut Frame, area: Rect, app: &App) {
    // Checked first. While filtering or searching, keys are absorbed into
    // the buffer instead of doing what the normal footer hints claim.
    let text = if let Some(prompt) = app.input_prompt() {
        format!("{prompt}   esc cancel \u{00b7} enter apply")
    } else {
        app.status_line().map_or_else(
            || default_footer(app.level_kind(), app),
            std::string::ToString::to_string,
        )
    };
    frame.render_widget(ratatui::widgets::Paragraph::new(text), area);
}

fn default_footer(level: LevelKind, app: &App) -> String {
    match level {
        LevelKind::Jobs => {
            "\u{23ce} open  r run  e edit  d enable/disable  s sort  / filter  q quit".to_string()
        }
        LevelKind::Runs => {
            if matches!(app.selected_run(), Some(r) if r.status == nightjar_store::run::RunStatus::Running)
            {
                "\u{23ce} output  \u{2190} back  r rerun  x kill  q quit".to_string()
            } else {
                "\u{23ce} output  \u{2190} back  r rerun  q quit".to_string()
            }
        }
        LevelKind::Output => {
            "\u{2190} back  tab stdout/stderr  / search  g/G top/bottom  q quit".to_string()
        }
    }
}

pub(crate) fn title_for(app: &App) -> String {
    match app.level_kind() {
        LevelKind::Jobs => "NIGHTJAR \u{2500} jobs".to_string(),
        LevelKind::Runs => format!(
            "NIGHTJAR \u{2500} jobs \u{203a} {}",
            app.current_job_name().unwrap_or("?")
        ),
        LevelKind::Output => format!(
            "NIGHTJAR \u{2500} jobs \u{203a} {} \u{203a} {} \u{2500} {}",
            app.current_job_name().unwrap_or("?"),
            app.current_run_id().unwrap_or("?"),
            match app.stream() {
                crate::app::Stream::Stdout => "stdout",
                crate::app::Stream::Stderr => "stderr",
            }
        ),
    }
}
