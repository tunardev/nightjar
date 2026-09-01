use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use jiff::Timestamp;
use nightjar_store::run::{RunStatus, Trigger};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Name,
    ProblemsFirst,
    LastRun,
}

impl SortMode {
    fn next(self) -> Self {
        match self {
            SortMode::Name => SortMode::ProblemsFirst,
            SortMode::ProblemsFirst => SortMode::LastRun,
            SortMode::LastRun => SortMode::Name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Level {
    Jobs,
    Runs { job: String },
    Output { job: String, run_id: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LevelKind {
    Jobs,
    Runs,
    Output,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Filter,
    Search,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    None,
    Quit,
    RunNow {
        job: String,
    },
    Edit {
        job: String,
    },
    ToggleEnabled {
        job: String,
    },
    Kill {
        job: String,
        run_id: String,
        pid: u32,
    },
    /// `App` doesn't hold the output text — `views::output` does. So the
    /// jump itself happens in the crate's event loop, not here.
    JumpToSearchMatch,
}

#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: String,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<i64>,
    pub status: RunStatus,
    pub trigger: Trigger,
    pub pid: Option<u32>,
    pub stdout_path: Option<PathBuf>,
    pub stderr_path: Option<PathBuf>,
}

impl From<nightjar_store::run::Run> for RunRow {
    fn from(r: nightjar_store::run::Run) -> Self {
        RunRow {
            id: r.id,
            started_at: r.started_at,
            finished_at: r.finished_at,
            exit_code: r.exit_code,
            duration_ms: r.duration_ms,
            status: r.status,
            trigger: r.trigger,
            pid: r.pid,
            stdout_path: r.stdout_path,
            stderr_path: r.stderr_path,
        }
    }
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub name: String,
    /// `None` means the job has no `schedule` field (e.g. `after`-triggered)
    /// or the TOML failed to load. `error` says which one.
    pub schedule: Option<String>,
    pub error: Option<String>,
    pub enabled: bool,
    pub last_run: Option<RunRow>,
    pub next: Option<Timestamp>,
    pub overdue_since: Option<Timestamp>,
}

#[derive(Debug, Clone)]
pub struct AppState {
    pub jobs_dir: PathBuf,
    pub jobs: Vec<JobRow>,
    pub runs: BTreeMap<String, Vec<RunRow>>,
    pub now: Timestamp,
}

impl AppState {
    fn runs_for<'a>(&'a self, job: &str) -> &'a [RunRow] {
        self.runs.get(job).map_or(&[], Vec::as_slice)
    }
}

pub struct App {
    state: AppState,
    stack: Vec<Level>,
    jobs_cursor: usize,
    runs_cursor: usize,
    filter: String,
    search: String,
    input_mode: Option<InputMode>,
    sort: SortMode,
    stream: Stream,
    output_scroll: usize,
    status: Option<String>,
}

impl App {
    pub fn new(state: AppState) -> Self {
        App {
            state,
            stack: vec![Level::Jobs],
            jobs_cursor: 0,
            runs_cursor: 0,
            filter: String::new(),
            search: String::new(),
            input_mode: None,
            sort: SortMode::Name,
            stream: Stream::Stdout,
            output_scroll: 0,
            status: None,
        }
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    pub fn sort(&self) -> SortMode {
        self.sort
    }

    pub fn stream(&self) -> Stream {
        self.stream
    }

    pub fn filter_text(&self) -> &str {
        &self.filter
    }

    pub fn search_text(&self) -> &str {
        &self.search
    }

    pub fn is_filtering(&self) -> bool {
        self.input_mode.is_some()
    }

    pub fn status_line(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = Some(msg.into());
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }

    pub fn jobs_dir(&self) -> &std::path::Path {
        &self.state.jobs_dir
    }

    pub fn output_scroll(&self) -> usize {
        self.output_scroll
    }

    pub fn set_output_scroll(&mut self, line: usize) {
        self.output_scroll = line;
    }

    pub fn input_prompt(&self) -> Option<String> {
        match self.input_mode {
            Some(InputMode::Filter) => Some(format!("filter: {}\u{2588}", self.filter)),
            Some(InputMode::Search) => Some(format!("search: {}\u{2588}", self.search)),
            None => None,
        }
    }

    pub fn any_run_in_flight(&self) -> bool {
        self.state
            .jobs
            .iter()
            .any(|j| matches!(j.last_run, Some(ref r) if r.status == RunStatus::Running))
    }

    pub fn update_state(&mut self, state: AppState) {
        self.state = state;
        loop {
            let still_valid = match self.stack.last() {
                None | Some(Level::Jobs) => true,
                Some(Level::Runs { job }) => self.state.jobs.iter().any(|j| &j.name == job),
                Some(Level::Output { job, run_id }) => {
                    self.state.runs_for(job).iter().any(|r| &r.id == run_id)
                }
            };
            if still_valid || self.stack.len() <= 1 {
                break;
            }
            self.stack.pop();
        }
        self.clamp_cursors();
    }

    fn clamp_cursors(&mut self) {
        let jobs_len = self.visible_jobs().len();
        self.jobs_cursor = clamp_index(self.jobs_cursor, jobs_len);
        if let Some(Level::Runs { job }) = self.stack.last().cloned() {
            let runs_len = self.state.runs_for(&job).len();
            self.runs_cursor = clamp_index(self.runs_cursor, runs_len);
        }
    }

    pub fn visible_jobs(&self) -> Vec<&JobRow> {
        let needle = self.filter.to_lowercase();
        let mut jobs: Vec<&JobRow> = self
            .state
            .jobs
            .iter()
            .filter(|j| needle.is_empty() || j.name.to_lowercase().contains(&needle))
            .collect();

        match self.sort {
            SortMode::Name => jobs.sort_by(|a, b| a.name.cmp(&b.name)),
            SortMode::ProblemsFirst => jobs.sort_by(|a, b| {
                problem_rank(a)
                    .cmp(&problem_rank(b))
                    .then_with(|| a.name.cmp(&b.name))
            }),
            SortMode::LastRun => jobs.sort_by(|a, b| {
                let by_time = last_run_key(b).cmp(&last_run_key(a));
                by_time.then_with(|| a.name.cmp(&b.name))
            }),
        }
        jobs
    }

    pub fn selected_job(&self) -> Option<&JobRow> {
        self.visible_jobs().into_iter().nth(self.jobs_cursor)
    }

    pub fn current_job_name(&self) -> Option<&str> {
        match self.stack.last() {
            Some(Level::Runs { job } | Level::Output { job, .. }) => Some(job.as_str()),
            _ => None,
        }
    }

    pub fn visible_runs(&self) -> &[RunRow] {
        match self.current_job_name() {
            Some(job) => self.state.runs_for(job),
            None => &[],
        }
    }

    pub fn selected_run(&self) -> Option<&RunRow> {
        self.visible_runs().get(self.runs_cursor)
    }

    pub fn level_kind(&self) -> LevelKind {
        match self.stack.last() {
            Some(Level::Jobs) | None => LevelKind::Jobs,
            Some(Level::Runs { .. }) => LevelKind::Runs,
            Some(Level::Output { .. }) => LevelKind::Output,
        }
    }

    pub fn current_run_id(&self) -> Option<&str> {
        match self.stack.last() {
            Some(Level::Output { run_id, .. }) => Some(run_id.as_str()),
            _ => None,
        }
    }

    /// Looked up by id, not by `selected_run`'s cursor. A poll can reorder
    /// or trim the runs list under an already-open output view.
    pub fn output_run(&self) -> Option<&RunRow> {
        match self.stack.last() {
            Some(Level::Output { job, run_id }) => {
                self.state.runs_for(job).iter().find(|r| &r.id == run_id)
            }
            _ => None,
        }
    }

    /// True only for the run currently open, unlike `any_run_in_flight`.
    pub fn viewing_running_output(&self) -> bool {
        matches!(self.output_run(), Some(r) if r.status == RunStatus::Running)
    }

    pub fn selected_index(&self) -> usize {
        match self.stack.last() {
            Some(Level::Runs { .. }) => self.runs_cursor,
            _ => self.jobs_cursor,
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        // Cleared here because nothing else ever clears it.
        self.status = None;
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Action::Quit;
        }
        if let Some(mode) = self.input_mode {
            return self.on_key_input(mode, key);
        }
        match self.stack.last().cloned() {
            Some(Level::Jobs) | None => self.on_key_jobs(key),
            Some(Level::Runs { job }) => self.on_key_runs(key, &job),
            Some(Level::Output { job, run_id }) => self.on_key_output(key, &job, &run_id),
        }
    }

    fn on_key_input(&mut self, mode: InputMode, key: KeyEvent) -> Action {
        let buf = match mode {
            InputMode::Filter => &mut self.filter,
            InputMode::Search => &mut self.search,
        };
        let mut action = Action::None;
        match key.code {
            KeyCode::Char(c) => buf.push(c),
            KeyCode::Backspace => {
                buf.pop();
            }
            KeyCode::Esc => {
                buf.clear();
                self.input_mode = None;
            }
            KeyCode::Enter => {
                self.input_mode = None;
                if mode == InputMode::Search && !self.search.is_empty() {
                    action = Action::JumpToSearchMatch;
                }
            }
            _ => {}
        }
        self.clamp_cursors();
        action
    }

    fn back_or_quit(&mut self) -> Action {
        if self.stack.len() > 1 {
            self.stack.pop();
            self.clamp_cursors();
            Action::None
        } else {
            Action::Quit
        }
    }

    fn move_cursor(cursor: &mut usize, len: usize, forward: bool) {
        if len == 0 {
            *cursor = 0;
            return;
        }
        *cursor = if forward {
            (*cursor + 1).min(len - 1)
        } else {
            cursor.saturating_sub(1)
        };
    }

    fn on_key_jobs(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                let len = self.visible_jobs().len();
                Self::move_cursor(&mut self.jobs_cursor, len, false);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let len = self.visible_jobs().len();
                Self::move_cursor(&mut self.jobs_cursor, len, true);
                Action::None
            }
            KeyCode::Enter => {
                if let Some(job) = self.selected_job() {
                    let name = job.name.clone();
                    self.stack.push(Level::Runs { job: name });
                    self.runs_cursor = 0;
                }
                Action::None
            }
            KeyCode::Char('/') => {
                self.input_mode = Some(InputMode::Filter);
                Action::None
            }
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                Action::None
            }
            KeyCode::Char('r') => self
                .selected_job()
                .map_or(Action::None, |j| Action::RunNow {
                    job: j.name.clone(),
                }),
            KeyCode::Char('e') => self.selected_job().map_or(Action::None, |j| Action::Edit {
                job: j.name.clone(),
            }),
            KeyCode::Char('d') => {
                self.selected_job()
                    .map_or(Action::None, |j| Action::ToggleEnabled {
                        job: j.name.clone(),
                    })
            }
            KeyCode::Left | KeyCode::Char('q') => self.back_or_quit(),
            _ => Action::None,
        }
    }

    fn on_key_runs(&mut self, key: KeyEvent, job: &str) -> Action {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                let len = self.visible_runs().len();
                Self::move_cursor(&mut self.runs_cursor, len, false);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let len = self.visible_runs().len();
                Self::move_cursor(&mut self.runs_cursor, len, true);
                Action::None
            }
            KeyCode::Enter => {
                if let Some(run) = self.selected_run() {
                    let run_id = run.id.clone();
                    self.stack.push(Level::Output {
                        job: job.to_string(),
                        run_id,
                    });
                    self.output_scroll = 0;
                    self.stream = Stream::Stdout;
                }
                Action::None
            }
            KeyCode::Char('r') => Action::RunNow {
                job: job.to_string(),
            },
            KeyCode::Char('x') => match self.selected_run() {
                Some(run) if run.status == RunStatus::Running => match run.pid {
                    Some(pid) => Action::Kill {
                        job: job.to_string(),
                        run_id: run.id.clone(),
                        pid,
                    },
                    None => Action::None,
                },
                _ => Action::None,
            },
            KeyCode::Left | KeyCode::Char('q') => self.back_or_quit(),
            _ => Action::None,
        }
    }

    fn on_key_output(&mut self, key: KeyEvent, _job: &str, _run_id: &str) -> Action {
        match key.code {
            KeyCode::Tab => {
                self.stream = match self.stream {
                    Stream::Stdout => Stream::Stderr,
                    Stream::Stderr => Stream::Stdout,
                };
                Action::None
            }
            KeyCode::Char('/') => {
                self.input_mode = Some(InputMode::Search);
                Action::None
            }
            KeyCode::Char('g') => {
                self.output_scroll = 0;
                Action::None
            }
            KeyCode::Char('G') => {
                self.output_scroll = usize::MAX;
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.output_scroll = self.output_scroll.saturating_sub(1);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.output_scroll = self.output_scroll.saturating_add(1);
                Action::None
            }
            KeyCode::Left | KeyCode::Char('q') => self.back_or_quit(),
            _ => Action::None,
        }
    }
}

fn clamp_index(index: usize, len: usize) -> usize {
    if len == 0 { 0 } else { index.min(len - 1) }
}

fn problem_rank(job: &JobRow) -> u8 {
    if job.error.is_some() {
        0
    } else if matches!(
        job.last_run,
        Some(ref r) if matches!(r.status, RunStatus::Failure | RunStatus::Timeout | RunStatus::Unknown)
    ) {
        1
    } else if job.overdue_since.is_some() {
        2
    } else {
        3
    }
}

/// `None` (never run) must sort last, in either direction. Wrapping
/// `Option<Timestamp>` directly in `Reverse` would sort it first instead.
fn last_run_key(job: &JobRow) -> Option<Timestamp> {
    job.last_run.as_ref().map(|r| r.started_at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn run(id: &str, started: &str, status: RunStatus) -> RunRow {
        RunRow {
            id: id.to_string(),
            started_at: started.parse().unwrap(),
            finished_at: Some(started.parse().unwrap()),
            exit_code: Some(0),
            duration_ms: Some(100),
            status,
            trigger: Trigger::Schedule,
            pid: None,
            stdout_path: None,
            stderr_path: None,
        }
    }

    fn job(name: &str) -> JobRow {
        JobRow {
            name: name.to_string(),
            schedule: Some("hourly".to_string()),
            error: None,
            enabled: true,
            last_run: None,
            next: None,
            overdue_since: None,
        }
    }

    fn state_with_jobs(names: &[&str]) -> AppState {
        AppState {
            jobs_dir: PathBuf::from("/tmp/jobs"),
            jobs: names.iter().map(|n| job(n)).collect(),
            runs: BTreeMap::new(),
            now: "2026-06-01T00:00:00Z".parse().unwrap(),
        }
    }

    fn fixture_state() -> AppState {
        let mut runs = BTreeMap::new();
        runs.insert(
            "backup".to_string(),
            vec![run("r1", "2026-06-01T00:00:00Z", RunStatus::Success)],
        );
        AppState {
            jobs_dir: PathBuf::from("/tmp/jobs"),
            jobs: vec![job("backup")],
            runs,
            now: "2026-06-01T01:00:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn enter_descends_and_q_returns_then_quits_at_the_top() {
        let mut app = App::new(fixture_state());
        assert_eq!(app.depth(), 1);

        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.depth(), 2);

        app.on_key(key(KeyCode::Char('q')));
        assert_eq!(app.depth(), 1);

        assert!(
            matches!(app.on_key(key(KeyCode::Char('q'))), Action::Quit),
            "q at level 1 quits — there is no separate escape to memorise"
        );
    }

    #[test]
    fn movement_accepts_both_arrows_and_vim_keys_and_clamps_at_the_ends() {
        let mut app = App::new(state_with_jobs(&["a", "b", "c"]));
        assert_eq!(app.selected_index(), 0);

        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected_index(), 1);
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.selected_index(), 2);
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.selected_index(), 2, "must clamp at the last row");

        app.on_key(key(KeyCode::Up));
        assert_eq!(app.selected_index(), 1);
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.selected_index(), 0);
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.selected_index(), 0, "must clamp at the first row");
    }

    #[test]
    fn slash_filters_the_job_list_by_name() {
        let mut app = App::new(state_with_jobs(&["backup", "cleanup", "sync-photos"]));
        app.on_key(key(KeyCode::Char('/')));
        for c in "back".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        let names: Vec<&str> = app.visible_jobs().iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, vec!["backup"]);
    }

    #[test]
    fn s_cycles_sort_between_name_problems_first_and_last_run() {
        let mut app = App::new(fixture_state());
        assert_eq!(app.sort(), SortMode::Name);
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.sort(), SortMode::ProblemsFirst);
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.sort(), SortMode::LastRun);
        app.on_key(key(KeyCode::Char('s')));
        assert_eq!(app.sort(), SortMode::Name);
    }

    #[test]
    fn empty_job_list_names_the_directory_to_create_a_job_in() {
        let state = AppState {
            jobs_dir: PathBuf::from("/home/x/.config/nightjar/jobs"),
            jobs: Vec::new(),
            runs: BTreeMap::new(),
            now: "2026-06-01T00:00:00Z".parse().unwrap(),
        };
        let app = App::new(state);
        assert!(app.visible_jobs().is_empty());
        assert_eq!(
            app.jobs_dir(),
            std::path::Path::new("/home/x/.config/nightjar/jobs")
        );
    }

    #[test]
    fn tab_toggles_stdout_and_stderr() {
        let mut runs = BTreeMap::new();
        runs.insert(
            "backup".to_string(),
            vec![run("r1", "2026-06-01T00:00:00Z", RunStatus::Success)],
        );
        let state = AppState {
            jobs_dir: PathBuf::from("/tmp"),
            jobs: vec![job("backup")],
            runs,
            now: "2026-06-01T01:00:00Z".parse().unwrap(),
        };
        let mut app = App::new(state);
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.stream(), Stream::Stdout);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.stream(), Stream::Stderr);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.stream(), Stream::Stdout);
    }

    #[test]
    fn x_is_offered_only_when_a_run_is_actually_in_flight() {
        let mut runs = BTreeMap::new();
        runs.insert(
            "backup".to_string(),
            vec![run("r1", "2026-06-01T00:00:00Z", RunStatus::Success)],
        );
        let finished_state = AppState {
            jobs_dir: PathBuf::from("/tmp"),
            jobs: vec![job("backup")],
            runs,
            now: "2026-06-01T01:00:00Z".parse().unwrap(),
        };
        let mut app = App::new(finished_state);
        app.on_key(key(KeyCode::Enter));
        assert_eq!(
            app.on_key(key(KeyCode::Char('x'))),
            Action::None,
            "a finished run must not be killable"
        );

        let mut running_run = run("r2", "2026-06-01T00:00:00Z", RunStatus::Running);
        running_run.pid = Some(4242);
        let mut runs2 = BTreeMap::new();
        runs2.insert("backup".to_string(), vec![running_run]);
        let running_state = AppState {
            jobs_dir: PathBuf::from("/tmp"),
            jobs: vec![job("backup")],
            runs: runs2,
            now: "2026-06-01T01:00:00Z".parse().unwrap(),
        };
        let mut app2 = App::new(running_state);
        app2.on_key(key(KeyCode::Enter));
        assert_eq!(
            app2.on_key(key(KeyCode::Char('x'))),
            Action::Kill {
                job: "backup".to_string(),
                run_id: "r2".to_string(),
                pid: 4242,
            }
        );
    }

    #[test]
    fn status_line_is_cleared_when_the_next_key_is_pressed() {
        let mut app = App::new(state_with_jobs(&["a", "b"]));
        app.set_status("started");
        assert_eq!(app.status_line(), Some("started"));

        app.on_key(key(KeyCode::Down));
        assert_eq!(
            app.status_line(),
            None,
            "a status set by a previous action must not survive the next keypress"
        );
    }

    #[test]
    fn enter_returns_jump_to_search_match_when_the_search_query_is_committed() {
        let mut runs = BTreeMap::new();
        runs.insert(
            "backup".to_string(),
            vec![run("r1", "2026-06-01T00:00:00Z", RunStatus::Success)],
        );
        let state = AppState {
            jobs_dir: PathBuf::from("/tmp"),
            jobs: vec![job("backup")],
            runs,
            now: "2026-06-01T01:00:00Z".parse().unwrap(),
        };
        let mut app = App::new(state);
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Enter));

        app.on_key(key(KeyCode::Char('/')));
        assert!(app.is_filtering());
        for c in "err".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::JumpToSearchMatch,
            "committing a non-empty search query must ask the caller to resolve it,              not silently do nothing — App has no output text to search itself"
        );
        assert!(!app.is_filtering(), "Enter must exit the input mode");
        assert_eq!(
            app.search_text(),
            "err",
            "the query must survive for the jump to use"
        );
    }

    #[test]
    fn enter_does_nothing_when_the_search_query_is_empty() {
        let mut runs = BTreeMap::new();
        runs.insert(
            "backup".to_string(),
            vec![run("r1", "2026-06-01T00:00:00Z", RunStatus::Success)],
        );
        let state = AppState {
            jobs_dir: PathBuf::from("/tmp"),
            jobs: vec![job("backup")],
            runs,
            now: "2026-06-01T01:00:00Z".parse().unwrap(),
        };
        let mut app = App::new(state);
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Char('/')));
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Action::None,
            "an empty query has nothing to jump to"
        );
    }

    #[test]
    fn input_prompt_is_visible_and_shows_the_live_buffer_only_when_filtering() {
        let mut app = App::new(state_with_jobs(&["backup", "cleanup"]));
        assert_eq!(app.input_prompt(), None);

        app.on_key(key(KeyCode::Char('/')));
        app.on_key(key(KeyCode::Char('q')));
        let prompt = app
            .input_prompt()
            .expect("must show a prompt while filtering");
        assert!(
            prompt.contains('q'),
            "the prompt must reflect the buffer actually being typed, not a static label: {prompt}"
        );

        app.on_key(key(KeyCode::Esc));
        assert_eq!(
            app.input_prompt(),
            None,
            "leaving filter mode must remove the prompt"
        );
    }

    #[test]
    fn q_is_absorbed_into_the_buffer_and_not_treated_as_quit_when_filtering() {
        let mut app = App::new(state_with_jobs(&["backup", "cleanup"]));
        app.on_key(key(KeyCode::Char('/')));
        let result = app.on_key(key(KeyCode::Char('q')));
        assert_eq!(
            result,
            Action::None,
            "q while filtering must be consumed by the filter text, never quit"
        );
        assert_eq!(app.filter_text(), "q");
        assert_eq!(app.depth(), 1, "must not have quit or changed level");
    }
}
