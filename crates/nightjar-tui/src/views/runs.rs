use crate::app::App;
use nightjar_core::format::{duration_human, relative_time};
use nightjar_store::run::RunStatus;
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};

const COLUMN_WIDTHS: [Constraint; 4] = [
    Constraint::Length(18),
    Constraint::Length(8),
    Constraint::Length(10),
    Constraint::Min(10),
];

pub fn render_runs(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(super::title_for(app));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let runs = app.visible_runs();
    if runs.is_empty() {
        let job = app.current_job_name().unwrap_or("?");
        frame.render_widget(Paragraph::new(format!("no runs recorded for {job}")), inner);
        return;
    }

    let selected = app.selected_index();
    let now = app.state().now;
    let header = Row::new(vec!["WHEN", "EXIT", "DURATION", "TRIGGER"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = runs
        .iter()
        .enumerate()
        .map(|(i, run)| run_row(run, now, i == selected))
        .collect();

    let table = Table::new(rows, COLUMN_WIDTHS).header(header);
    frame.render_widget(table, inner);
}

fn run_row(run: &crate::app::RunRow, now: jiff::Timestamp, selected: bool) -> Row<'static> {
    let marker = if selected { "\u{25b8} " } else { "  " };
    let when = format!("{marker}{}", relative_time(run.started_at, now));

    let (label, color) = match run.status {
        RunStatus::Success => ("ok", Color::Green),
        RunStatus::Running => ("\u{2026}", Color::Reset),
        RunStatus::Timeout => ("TIMEOUT", Color::Red),
        RunStatus::Unknown => ("UNKNOWN", Color::Yellow),
        RunStatus::Missed => ("MISSED", Color::Yellow),
        RunStatus::Failure => ("FAIL", Color::Red),
        RunStatus::Limit => ("LIMIT", Color::Red),
    };
    let duration = run
        .duration_ms
        .map_or_else(|| "\u{2014}".to_string(), duration_human);

    let cells = vec![
        Cell::from(when),
        Cell::from(label).style(Style::default().fg(color)),
        Cell::from(duration),
        Cell::from(run.trigger.to_db_string()),
    ];
    let row = Row::new(cells);
    if selected {
        row.style(Style::default().add_modifier(Modifier::REVERSED))
    } else {
        row
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, AppState, JobRow, RunRow};
    use nightjar_store::run::Trigger;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn run(id: &str) -> RunRow {
        RunRow {
            id: id.to_string(),
            started_at: "2026-06-01T00:00:00Z".parse().unwrap(),
            finished_at: Some("2026-06-01T00:00:01Z".parse().unwrap()),
            exit_code: Some(1),
            duration_ms: Some(300),
            status: RunStatus::Failure,
            trigger: Trigger::Schedule,
            pid: None,
            stdout_path: None,
            stderr_path: None,
        }
    }

    fn app_at_runs(job_name: &str, runs: Vec<RunRow>) -> App {
        let mut map = BTreeMap::new();
        map.insert(job_name.to_string(), runs);
        let state = AppState {
            jobs_dir: PathBuf::from("/tmp"),
            jobs: vec![JobRow {
                name: job_name.to_string(),
                schedule: Some("hourly".to_string()),
                error: None,
                enabled: true,
                last_run: None,
                next: None,
                overdue_since: None,
            }],
            runs: map,
            now: "2026-06-01T01:00:00Z".parse().unwrap(),
        };
        let mut app = App::new(state);
        app.on_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Enter,
            crossterm::event::KeyModifiers::NONE,
        ));
        app
    }

    #[test]
    fn renders_the_required_columns_and_an_empty_job_says_so() {
        let app = app_at_runs("cleanup", vec![run("r1")]);
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_runs(f, f.area(), &app)).unwrap();
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("WHEN"));
        assert!(text.contains("TRIGGER"));
        assert!(text.contains("FAIL"));
        assert!(text.contains("schedule"));

        let empty = app_at_runs("cleanup", Vec::new());
        let backend2 = TestBackend::new(80, 24);
        let mut terminal2 = Terminal::new(backend2).unwrap();
        terminal2
            .draw(|f| render_runs(f, f.area(), &empty))
            .unwrap();
        let text2 = buffer_text(terminal2.backend().buffer());
        assert!(text2.contains("no runs recorded for cleanup"), "{text2}");
    }
}
