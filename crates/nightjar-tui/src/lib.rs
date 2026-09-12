pub mod actions;
pub mod app;
pub mod term;
pub mod views;

use std::io;

use anyhow::Result;
use nightjar_core::paths::Paths;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// # Errors
/// fails if the terminal or signal handlers cannot be set up, or the event loop errors
pub fn cmd_tui() -> Result<i32> {
    let paths = Paths::resolve()?;
    let signals = term::install_tui_signal_handlers()?;

    let mut guard = term::enter_tui(term::CrosstermRaw, term::install_panic_hook)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    let result = run(&mut terminal, &mut guard, &paths, &signals);

    drop(guard);
    result
}

fn run<T: term::RawTerminal>(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    guard: &mut term::TerminalGuard<T>,
    paths: &Paths,
    signals: &term::SignalFlags,
) -> Result<i32> {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use crossterm::event::{self, Event, KeyEventKind};

    let store = nightjar_store::Store::open(&paths.db_path)?;
    let clock = nightjar_core::clock::SystemClock;
    let mut app = app::App::new(load_state(&store, paths, &clock)?);
    let mut last_data_version = store.data_version()?;
    let mut spawned: Vec<std::process::Child> = Vec::new();

    let inject_test_panic = std::env::var_os("NIGHTJAR_TUI_TEST_PANIC").is_some();

    terminal.draw(|f| views::draw(f, &app))?;
    assert!(!inject_test_panic, "nightjar tui: injected test panic");

    loop {
        if signals.shutdown.load(Ordering::SeqCst) {
            return Ok(0);
        }

        spawned.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));

        let resized = signals.resize.swap(false, Ordering::SeqCst);
        if resized {
            terminal.autoresize()?;
        }
        let mut dirty = resized;

        let poll_for = if app.any_run_in_flight() {
            Duration::from_millis(500)
        } else {
            Duration::from_secs(2)
        };

        if event::poll(poll_for)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            dirty = true;
            match app.on_key(key) {
                app::Action::Quit => return Ok(0),
                app::Action::RunNow { job } => match actions::run_now(paths, &job) {
                    Ok(child) => {
                        spawned.push(child);
                        app.set_status(format!("{job}: started"));
                    }
                    Err(e) => app.set_status(format!("{job}: {e:#}")),
                },
                app::Action::Edit { job } => {
                    let outcome = actions::suspend_and_edit(guard, || actions::edit(&job));
                    terminal.clear()?;
                    app.update_state(load_state(&store, paths, &clock)?);
                    last_data_version = store.data_version()?;
                    if let Err(e) = outcome {
                        app.set_status(format!("{job}: {e:#}"));
                    }
                }
                app::Action::ToggleEnabled { job } => match actions::toggle_enabled(paths, &job) {
                    Ok(now_enabled) => app.set_status(format!(
                        "{job}: {}",
                        if now_enabled { "enabled" } else { "disabled" }
                    )),
                    Err(e) => app.set_status(format!("{job}: {e:#}")),
                },
                app::Action::Kill { pid, .. } => {
                    if let Err(e) = actions::kill_run(pid) {
                        app.set_status(format!("kill failed: {e:#}"));
                    }
                }
                app::Action::JumpToSearchMatch => jump_to_search_match(&mut app),
                app::Action::None => {}
            }
        }

        let version = store.data_version()?;
        if version != last_data_version {
            app.update_state(load_state(&store, paths, &clock)?);
            last_data_version = version;
            dirty = true;
        } else if app.viewing_running_output() {
            dirty = true;
        }

        if dirty {
            terminal.draw(|f| views::draw(f, &app))?;
        }
    }
}

fn jump_to_search_match(app: &mut app::App) {
    let path = app.output_run().and_then(|run| match app.stream() {
        app::Stream::Stdout => run.stdout_path.clone(),
        app::Stream::Stderr => run.stderr_path.clone(),
    });
    let query = app.search_text().to_string();
    let current = app.output_scroll();

    match views::output::load_output(path.as_deref()) {
        Ok(views::output::OutputContent::Loaded(loaded)) => {
            let drawn = loaded.display_lines();
            match views::output::find_next_match(&drawn, &query, current) {
                Some(row) => app.set_output_scroll(row),
                None => app.set_status(format!("no match for {query:?}")),
            }
        }
        Ok(views::output::OutputContent::NoOutput | views::output::OutputContent::Pruned) => {
            app.set_status("nothing to search".to_string());
        }
        Err(e) => app.set_status(format!("search failed: {e:#}")),
    }
}

fn load_state(
    store: &nightjar_store::Store,
    paths: &Paths,
    clock: &dyn nightjar_core::clock::Clock,
) -> Result<app::AppState> {
    use std::collections::BTreeMap;

    use nightjar_config::job::{JobsDirState, probe_jobs_dir};
    use nightjar_config::{Config, Job};
    use nightjar_store::overdue_since;

    let now = clock.now();
    let config = Config::load(paths)?;

    let loaded: Vec<(String, Result<Job>)> = match probe_jobs_dir(&paths.jobs_dir)? {
        JobsDirState::Missing => Vec::new(),
        JobsDirState::Present => Job::load_all(&paths.jobs_dir),
    };

    let mut jobs = Vec::with_capacity(loaded.len());
    let mut runs = BTreeMap::new();

    for (name, result) in loaded {
        let job_runs = store.recent_runs(Some(&name), config.retention_runs)?;
        let row = match result {
            Ok(job) => {
                let state = store.job_state(&name)?;
                let last = job_runs.first().cloned();
                let overdue = overdue_since(state.as_ref(), last.as_ref(), now);
                app::JobRow {
                    name: name.clone(),
                    schedule: job.schedule_source().map(String::from),
                    error: None,
                    enabled: job.enabled,
                    last_run: last.map(app::RunRow::from),
                    next: state.and_then(|s| s.next_run_at),
                    overdue_since: overdue,
                }
            }
            Err(e) => app::JobRow {
                name: name.clone(),
                schedule: None,
                error: Some(nightjar_core::format::error_summary(&e)),
                enabled: false,
                last_run: job_runs.first().cloned().map(app::RunRow::from),
                next: None,
                overdue_since: None,
            },
        };
        jobs.push(row);
        runs.insert(name, job_runs.into_iter().map(app::RunRow::from).collect());
    }

    Ok(app::AppState {
        jobs_dir: paths.jobs_dir.clone(),
        jobs,
        runs,
        now,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use nightjar_core::clock::SystemClock;
    use nightjar_core::paths::Paths;
    use nightjar_store::run::{RunStatus, Trigger};

    use super::{jump_to_search_match, load_state};
    use crate::app::{App, AppState, JobRow, RunRow};
    use crate::views::output::{OutputContent, load_output};

    fn app_viewing(path: &Path) -> App {
        let run = RunRow {
            id: "r1".to_string(),
            started_at: "2026-06-01T00:00:00Z".parse().unwrap(),
            finished_at: Some("2026-06-01T00:00:01Z".parse().unwrap()),
            exit_code: Some(0),
            duration_ms: Some(1),
            status: RunStatus::Success,
            trigger: Trigger::Schedule,
            pid: None,
            stdout_path: Some(path.to_path_buf()),
            stderr_path: None,
        };
        let mut runs = BTreeMap::new();
        runs.insert("backup".to_string(), vec![run.clone()]);
        let mut app = App::new(AppState {
            jobs_dir: PathBuf::from("/tmp"),
            jobs: vec![JobRow {
                name: "backup".to_string(),
                schedule: Some("hourly".to_string()),
                error: None,
                enabled: true,
                last_run: Some(run),
                next: None,
                overdue_since: None,
            }],
            runs,
            now: "2026-06-01T01:00:00Z".parse().unwrap(),
        });
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        app
    }

    fn search_for(app: &mut App, needle: &str) {
        app.on_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for c in needle.chars() {
            app.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        jump_to_search_match(app);
    }

    fn drawn_line_at(path: &Path, index: usize) -> String {
        let OutputContent::Loaded(loaded) = load_output(Some(path)).unwrap() else {
            panic!("expected Loaded")
        };
        loaded.display_lines()[index].to_string()
    }

    #[test]
    fn search_scrolls_to_the_line_the_pane_draws_the_match_on() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("small.out");
        std::fs::write(&path, "alpha\nNEEDLE\ngamma\n").unwrap();

        let mut app = app_viewing(&path);
        search_for(&mut app, "NEEDLE");

        assert_eq!(drawn_line_at(&path, app.output_scroll()), "NEEDLE");
    }

    #[test]
    fn search_scrolls_to_the_drawn_line_even_when_a_tail_label_shifts_every_row_down() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.out");
        let mut body = "filler\n".repeat(200_000);
        body.push_str("NEEDLE\n");
        std::fs::write(&path, &body).unwrap();

        let mut app = app_viewing(&path);
        search_for(&mut app, "NEEDLE");

        assert_eq!(
            drawn_line_at(&path, app.output_scroll()),
            "NEEDLE",
            "the tail banner occupies a drawn row, so a match found in the text alone \
             lands one row above the line the pane scrolls to"
        );
    }

    #[test]
    fn search_from_the_bottom_of_the_output_wraps_instead_of_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("small.out");
        std::fs::write(&path, "alpha\nNEEDLE\ngamma\n").unwrap();

        let mut app = app_viewing(&path);
        app.on_key(KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE));
        search_for(&mut app, "NEEDLE");

        assert_eq!(drawn_line_at(&path, app.output_scroll()), "NEEDLE");
    }

    #[test]
    fn malformed_config_is_an_error_not_silently_defaulted() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        std::fs::write(
            paths.config_dir.join("config.toml"),
            "retention_runs = = 5\n",
        )
        .unwrap();
        let store = nightjar_store::Store::open(&paths.db_path).unwrap();

        let err = load_state(&store, &paths, &SystemClock).unwrap_err();
        assert!(err.to_string().contains("config.toml"), "got: {err:#}");
    }

    #[test]
    fn state_loads_when_config_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        let store = nightjar_store::Store::open(&paths.db_path).unwrap();

        assert!(load_state(&store, &paths, &SystemClock).is_ok());
    }
}
