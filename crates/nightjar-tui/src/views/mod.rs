pub mod jobs;
pub mod output;
pub mod runs;

use nightjar_store::run::RunStatus;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::widgets::Cell;

use crate::app::{App, LevelKind};

pub(crate) struct StatusBadge {
    label: &'static str,
    color: Color,
}

impl StatusBadge {
    pub(crate) fn cell(&self) -> Cell<'static> {
        Cell::from(self.label).style(Style::default().fg(self.color))
    }
}

pub(crate) const fn status_badge(status: RunStatus) -> StatusBadge {
    let (label, color) = match status {
        RunStatus::Success => ("ok", Color::Green),
        RunStatus::Running => ("\u{2026}", Color::Reset),
        RunStatus::Timeout => ("TIMEOUT", Color::Red),
        RunStatus::Unknown => ("UNKNOWN", Color::Yellow),
        RunStatus::Missed => ("MISSED", Color::Yellow),
        RunStatus::Failure => ("FAIL", Color::Red),
        RunStatus::Limit => ("LIMIT", Color::Red),
    };
    StatusBadge { label, color }
}

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
    let text = app.input_prompt().map_or_else(
        || {
            app.status_line().map_or_else(
                || default_footer(app.level_kind(), app),
                std::string::ToString::to_string,
            )
        },
        |prompt| format!("{prompt}   esc cancel  enter apply"),
    );
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use nightjar_store::run::Trigger;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;
    use crate::app::{AppState, JobRow, RunRow};

    fn run(id: &str, status: RunStatus) -> RunRow {
        RunRow {
            id: id.to_string(),
            started_at: "2026-06-01T00:00:00Z".parse().unwrap(),
            finished_at: Some("2026-06-01T00:00:12Z".parse().unwrap()),
            exit_code: Some(0),
            duration_ms: Some(12_400),
            status,
            trigger: Trigger::Manual,
            pid: None,
            stdout_path: None,
            stderr_path: None,
        }
    }

    fn app_with(status: RunStatus) -> App {
        let mut runs = BTreeMap::new();
        runs.insert("backup".to_string(), vec![run("r1", status)]);
        App::new(AppState {
            jobs_dir: PathBuf::from("/tmp/jobs"),
            jobs: vec![JobRow {
                name: "backup".to_string(),
                schedule: Some("hourly".to_string()),
                error: None,
                enabled: true,
                last_run: Some(run("r1", status)),
                next: None,
                overdue_since: None,
            }],
            runs,
            now: "2026-06-01T01:00:00Z".parse().unwrap(),
        })
    }

    fn descend(app: &mut App, levels: usize) {
        for _ in 0..levels {
            app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
    }

    fn screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn footer(app: &App) -> String {
        screen(app).lines().last().unwrap().trim_end().to_string()
    }

    #[test]
    fn every_level_offers_quit_and_the_keys_that_level_actually_has() {
        let mut app = app_with(RunStatus::Success);
        assert!(footer(&app).contains("r run"), "jobs: {}", footer(&app));

        descend(&mut app, 1);
        assert!(footer(&app).contains("r rerun"), "runs: {}", footer(&app));

        descend(&mut app, 1);
        let output = footer(&app);
        assert!(output.contains("tab stdout/stderr"), "output: {output}");
        assert!(output.contains("search"), "output: {output}");

        for level in [LevelKind::Jobs, LevelKind::Runs, LevelKind::Output] {
            let mut app = app_with(RunStatus::Success);
            descend(&mut app, level as usize);
            assert!(footer(&app).contains("q quit"), "got: {}", footer(&app));
        }
    }

    #[test]
    fn kill_is_offered_only_while_the_selected_run_is_still_running() {
        let mut running = app_with(RunStatus::Running);
        descend(&mut running, 1);
        assert!(footer(&running).contains("x kill"), "{}", footer(&running));

        let mut finished = app_with(RunStatus::Success);
        descend(&mut finished, 1);
        assert!(
            !footer(&finished).contains("x kill"),
            "a finished run has nothing to kill; got: {}",
            footer(&finished)
        );
    }

    #[test]
    fn a_status_message_takes_the_footer_over_from_the_key_hints() {
        let mut app = app_with(RunStatus::Success);
        app.set_status("started backup");

        assert_eq!(footer(&app).trim(), "started backup");
        assert!(
            !footer(&app).contains("q quit"),
            "the report replaces the hints rather than crowding in beside them"
        );
    }

    #[test]
    fn a_prompt_replaces_the_footer_and_names_both_ways_out_of_it() {
        let mut app = app_with(RunStatus::Success);
        app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));

        let prompt = footer(&app);
        assert!(prompt.contains("esc cancel"), "got: {prompt}");
        assert!(prompt.contains("enter apply"), "got: {prompt}");
    }

    #[test]
    fn the_title_says_where_you_are_at_every_depth() {
        let mut app = app_with(RunStatus::Success);
        assert!(screen(&app).contains("jobs"), "{}", screen(&app));

        descend(&mut app, 1);
        assert!(screen(&app).contains("backup"), "{}", screen(&app));

        descend(&mut app, 1);
        let output = screen(&app);
        assert!(output.contains("r1"), "{output}");
        assert!(output.contains("stdout"), "{output}");
    }

    #[test]
    fn a_badge_is_short_enough_for_its_column_and_never_blank() {
        for status in [
            RunStatus::Success,
            RunStatus::Running,
            RunStatus::Timeout,
            RunStatus::Unknown,
            RunStatus::Missed,
            RunStatus::Failure,
            RunStatus::Limit,
        ] {
            let badge = status_badge(status);
            assert!(!badge.label.is_empty(), "{status:?} has no badge");
            assert!(badge.label.chars().count() <= 7, "{status:?} is too wide");
        }
    }
}
