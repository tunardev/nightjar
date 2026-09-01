use crate::app::App;
use nightjar_core::format::{abbreviate_schedule, duration_human, relative_future, relative_time};
use nightjar_store::run::RunStatus;
use ratatui::Frame;
use ratatui::layout::{Constraint, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders, Cell, Row, Table};

const COLUMN_WIDTHS: [Constraint; 6] = [
    Constraint::Length(16),
    Constraint::Length(20),
    Constraint::Length(18),
    Constraint::Length(8),
    Constraint::Length(10),
    Constraint::Min(6),
];

pub fn render_jobs(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(super::title_for(app));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.state().jobs.is_empty() {
        let msg = format!(
            "no jobs configured yet (add a .toml file to {})",
            app.jobs_dir().display()
        );
        frame.render_widget(ratatui::widgets::Paragraph::new(msg), inner);
        return;
    }

    let jobs = app.visible_jobs();
    if jobs.is_empty() {
        frame.render_widget(
            ratatui::widgets::Paragraph::new("no jobs match the filter"),
            inner,
        );
        return;
    }

    let selected = app.selected_index();
    let now = app.state().now;
    let header = Row::new(vec![
        "JOB", "SCHEDULE", "LAST RUN", "EXIT", "DURATION", "NEXT",
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = jobs
        .iter()
        .enumerate()
        .map(|(i, job)| job_row(job, now, i == selected))
        .collect();

    let table = Table::new(rows, COLUMN_WIDTHS).header(header);
    frame.render_widget(table, inner);
}

fn job_row(job: &crate::app::JobRow, now: jiff::Timestamp, selected: bool) -> Row<'static> {
    let marker = if selected { "\u{25b8} " } else { "  " };
    let name = format!("{marker}{}", job.name);

    if let Some(err) = &job.error {
        let cells = vec![
            Cell::from(name),
            Cell::from("\u{2014}"),
            Cell::from("\u{2014}"),
            Cell::from("invalid").style(Style::default().fg(Color::Red)),
            Cell::from("\u{2014}"),
            Cell::from(err.clone()),
        ];
        return style_row(Row::new(cells), selected);
    }

    let schedule = job
        .schedule
        .as_deref()
        .map_or_else(|| "\u{2014}".to_string(), abbreviate_schedule);

    let (last_run, exit, duration) = match &job.last_run {
        None => (
            "never".to_string(),
            Cell::from("\u{2014}"),
            "\u{2014}".to_string(),
        ),
        Some(r) => {
            let when = relative_time(r.started_at, now);
            let (label, color) = match r.status {
                RunStatus::Success => ("ok", Color::Green),
                RunStatus::Running => ("\u{2026}", Color::Reset),
                RunStatus::Timeout => ("TIMEOUT", Color::Red),
                RunStatus::Unknown => ("UNKNOWN", Color::Yellow),
                RunStatus::Missed => ("MISSED", Color::Yellow),
                RunStatus::Failure => ("FAIL", Color::Red),
                RunStatus::Limit => ("LIMIT", Color::Red),
            };
            let dur = r
                .duration_ms
                .map_or_else(|| "\u{2014}".to_string(), duration_human);
            (
                when,
                Cell::from(label).style(Style::default().fg(color)),
                dur,
            )
        }
    };

    let next = if let Some(since) = job.overdue_since {
        Cell::from(format!("OVERDUE {}", relative_time(since, now)))
            .style(Style::default().fg(Color::Red))
    } else {
        match job.next {
            Some(t) => Cell::from(relative_future(t, now)),
            None => Cell::from("\u{2014}"),
        }
    };

    let cells = vec![
        Cell::from(name),
        Cell::from(schedule),
        Cell::from(last_run),
        exit,
        Cell::from(duration),
        next,
    ];
    style_row(Row::new(cells), selected)
}

fn style_row(row: Row<'static>, selected: bool) -> Row<'static> {
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

    fn job(name: &str, error: Option<&str>) -> JobRow {
        JobRow {
            name: name.to_string(),
            schedule: error.is_none().then(|| "hourly".to_string()),
            error: error.map(str::to_string),
            enabled: true,
            last_run: None,
            next: None,
            overdue_since: None,
        }
    }

    fn fully_populated_job() -> JobRow {
        JobRow {
            name: "backup".to_string(),
            schedule: Some("hourly".to_string()),
            error: None,
            enabled: true,
            last_run: Some(RunRow {
                id: "r1".to_string(),
                started_at: "2026-06-01T00:00:00Z".parse().unwrap(),
                finished_at: Some("2026-06-01T00:00:12Z".parse().unwrap()),
                exit_code: Some(0),
                duration_ms: Some(12_400),
                status: RunStatus::Success,
                trigger: Trigger::Schedule,
                pid: None,
                stdout_path: None,
                stderr_path: None,
            }),
            next: Some("2026-06-01T02:00:00Z".parse().unwrap()),
            overdue_since: None,
        }
    }

    fn state(jobs: Vec<JobRow>) -> AppState {
        AppState {
            jobs_dir: PathBuf::from("/tmp/jobs"),
            jobs,
            runs: BTreeMap::new(),
            now: "2026-06-01T01:00:00Z".parse().unwrap(),
        }
    }

    fn render_at(width: u16, height: u16, app: &App) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| render_jobs(f, f.area(), app)).unwrap();
        buffer_text(terminal.backend().buffer())
    }

    #[test]
    fn jobs_view_renders_at_eighty_columns_and_at_two_hundred() {
        let app = App::new(state(vec![
            fully_populated_job(),
            job("cleanup", None),
            job("broken", Some("parsing broken.toml: bad")),
        ]));

        for (w, h) in [(80u16, 24u16), (200u16, 50u16)] {
            let text = render_at(w, h, &app);
            for header in ["JOB", "SCHEDULE", "LAST RUN", "EXIT", "DURATION", "NEXT"] {
                assert!(
                    text.contains(header),
                    "width {w}: header {header:?} missing: {text}"
                );
            }

            let backup_line = text
                .lines()
                .find(|l| l.contains("backup"))
                .unwrap_or_else(|| panic!("width {w}: no rendered line contains backup: {text}"));
            for value in ["hourly", "1h ago", "ok", "12.4s", "in 1h"] {
                assert!(
                    backup_line.contains(value),
                    "width {w}: backup's row is missing {value:?}: {backup_line:?}"
                );
            }

            assert!(text.contains("cleanup"), "width {w}");
            assert!(text.contains("broken"), "width {w}");
            assert!(text.contains("invalid"), "width {w}: {text}");
        }
    }

    #[test]
    fn empty_job_list_names_the_directory_to_create_a_job_in() {
        let mut s = state(Vec::new());
        s.jobs_dir = PathBuf::from("/home/x/.config/nightjar/jobs");
        let app = App::new(s);
        let text = render_at(80, 24, &app);
        assert!(
            text.contains("/home/x/.config/nightjar/jobs"),
            "got: {text}"
        );
    }
}
