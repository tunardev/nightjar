pub mod actions;
pub mod app;
pub mod term;
pub mod views;

use anyhow::Result;
use nightjar_core::paths::Paths;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use std::io;

/// Never needs the daemon. Opens the same store the CLI does, read-only,
/// and polls it. This still works when the daemon is dead.
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
    use crossterm::event::{self, Event, KeyEventKind};
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    let store = nightjar_store::Store::open(&paths.db_path)?;
    let clock = nightjar_core::clock::SystemClock;
    let mut app = app::App::new(load_state(&store, paths, &clock)?);
    let mut last_data_version = data_version(&store)?;
    let mut spawned: Vec<std::process::Child> = Vec::new();

    // Lets a test trigger a panic here, to exercise the panic-hook restore.
    // Inert unless this env var is set, which normal runs never do.
    let inject_test_panic = std::env::var_os("NIGHTJAR_TUI_TEST_PANIC").is_some();

    terminal.draw(|f| views::draw(f, &app))?;
    assert!(!inject_test_panic, "nightjar tui: injected test panic");

    loop {
        if signals.shutdown.load(Ordering::SeqCst) {
            return Ok(0);
        }

        spawned.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_))));

        let mut dirty = false;
        if signals.resize.swap(false, Ordering::SeqCst) {
            terminal.autoresize()?;
            dirty = true;
        }

        // Tail-following output has no matching write to the `runs` table.
        // `data_version` alone can't see it.
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
                    // A filesystem write, not a database write. `data_version`
                    // would not see it. Reload unconditionally here.
                    app.update_state(load_state(&store, paths, &clock)?);
                    last_data_version = data_version(&store)?;
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

        let version = data_version(&store)?;
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

/// Lives here, not on `App`. The output text is loaded from disk by
/// `views::output`, not held in `AppState`.
fn jump_to_search_match(app: &mut app::App) {
    let path = app.output_run().and_then(|run| match app.stream() {
        app::Stream::Stdout => run.stdout_path.clone(),
        app::Stream::Stderr => run.stderr_path.clone(),
    });
    let query = app.search_text().to_string();
    let current = app.output_scroll();

    match views::output::load_output(path.as_deref()) {
        Ok(views::output::OutputContent::Loaded(loaded)) => {
            let lines: Vec<&str> = loaded.text.lines().collect();
            match views::output::find_next_match(&lines, &query, current) {
                Some(line) => app.set_output_scroll(line),
                None => app.set_status(format!("no match for {query:?}")),
            }
        }
        Ok(views::output::OutputContent::NoOutput | views::output::OutputContent::Pruned) => {
            app.set_status("nothing to search".to_string());
        }
        Err(e) => app.set_status(format!("search failed: {e:#}")),
    }
}

/// One integer read. This lets the common case, where nothing changed,
/// skip requerying jobs and runs entirely.
fn data_version(store: &nightjar_store::Store) -> Result<i64> {
    store.data_version()
}

fn load_state(
    store: &nightjar_store::Store,
    paths: &Paths,
    clock: &dyn nightjar_core::clock::Clock,
) -> Result<app::AppState> {
    use nightjar_config::job::{JobsDirState, probe_jobs_dir};
    use nightjar_config::{Config, Job};
    use nightjar_store::overdue_since;
    use std::collections::BTreeMap;

    let now = clock.now();
    // Not `unwrap_or_default`: the daemon refuses a malformed config, and a
    // view that quietly assumes defaults would show history pruned to a
    // limit the user never set.
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
mod load_state_tests {
    use super::load_state;
    use nightjar_core::clock::SystemClock;
    use nightjar_core::paths::Paths;

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
