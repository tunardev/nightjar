use crate::app::{App, Stream};
use anyhow::{Context, Result};
use nightjar_runner::capture::TRUNCATION_MARKER;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Distinct from the capture cap (`Config::output_cap`). That limits what
/// a run writes to disk. This limits what one draw reads back.
const VIEW_TAIL_THRESHOLD: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct LoadedOutput {
    pub text: String,
    pub label: Option<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub enum OutputContent {
    NoOutput,
    /// Must not render as if the run produced nothing.
    Pruned,
    Loaded(LoadedOutput),
}

pub fn load_output(path: Option<&Path>) -> Result<OutputContent> {
    let Some(path) = path else {
        return Ok(OutputContent::NoOutput);
    };

    let len = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(OutputContent::Pruned),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let read_result = if len <= VIEW_TAIL_THRESHOLD {
        std::fs::read(path).map(|bytes| (bytes, None))
    } else {
        read_tail(path, VIEW_TAIL_THRESHOLD).map(|bytes| {
            let label = format!("showing last 1MB \u{2014} full file at {}", path.display());
            (bytes, Some(label))
        })
    };

    let (bytes, label) = match read_result {
        Ok(v) => v,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(OutputContent::Pruned),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let truncated = contains_subslice(&bytes, TRUNCATION_MARKER.as_bytes());
    let text = String::from_utf8_lossy(&bytes).into_owned();
    Ok(OutputContent::Loaded(LoadedOutput {
        text,
        label,
        truncated,
    }))
}

fn read_tail(path: &Path, tail: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(tail);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

pub fn find_next_match(lines: &[&str], query: &str, after: usize) -> Option<usize> {
    if query.is_empty() || lines.is_empty() {
        return None;
    }
    let needle = query.to_lowercase();
    let len = lines.len();
    (1..=len)
        .map(|offset| (after + offset) % len)
        .find(|&idx| lines[idx].to_lowercase().contains(&needle))
}

pub fn render_output_view(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(super::title_for(app));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(run) = app.output_run() else {
        frame.render_widget(Paragraph::new("run no longer available"), inner);
        return;
    };

    let path = match app.stream() {
        Stream::Stdout => run.stdout_path.as_deref(),
        Stream::Stderr => run.stderr_path.as_deref(),
    };

    let content = match load_output(path) {
        Ok(c) => c,
        Err(e) => {
            frame.render_widget(
                Paragraph::new(format!("error reading output: {e:#}")),
                inner,
            );
            return;
        }
    };

    render_content(frame, inner, &content, app.output_scroll());
}

fn render_content(frame: &mut Frame, area: Rect, content: &OutputContent, scroll: usize) {
    let trimmed_marker = TRUNCATION_MARKER.trim_matches('\n');
    match content {
        OutputContent::NoOutput => {
            frame.render_widget(Paragraph::new("no output"), area);
        }
        OutputContent::Pruned => {
            frame.render_widget(
                Paragraph::new("output pruned").style(Style::default().fg(Color::Yellow)),
                area,
            );
        }
        OutputContent::Loaded(loaded) => {
            let mut lines: Vec<Line> = Vec::new();
            if let Some(label) = &loaded.label {
                lines.push(Line::from(Span::styled(
                    label.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::ITALIC),
                )));
            }
            for line in loaded.text.lines() {
                if line == trimmed_marker {
                    lines.push(Line::from(Span::styled(
                        line.to_string(),
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )));
                } else {
                    lines.push(Line::from(line.to_string()));
                }
            }
            let max_scroll = lines.len().saturating_sub(1);
            let offset = scroll.min(max_scroll);
            let offset = u16::try_from(offset).unwrap_or(u16::MAX);
            let paragraph = Paragraph::new(lines).scroll((offset, 0));
            frame.render_widget(paragraph, area);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn as_text(c: &OutputContent) -> &str {
        match c {
            OutputContent::Loaded(l) => &l.text,
            OutputContent::NoOutput | OutputContent::Pruned => panic!("expected Loaded"),
        }
    }

    fn render_to_text(content: &OutputContent, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_content(f, f.area(), content, 0))
            .unwrap();
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

    #[test]
    fn pruned_output_file_says_pruned_rather_than_showing_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gone.out");
        let content = load_output(Some(&path)).unwrap();
        assert!(matches!(content, OutputContent::Pruned));

        let text = render_to_text(&content, 40, 5);
        assert!(
            text.contains("output pruned"),
            "the screen must say the output was pruned, not just the enum; got: {text:?}"
        );
    }

    #[test]
    fn output_is_distinct_from_pruned_when_no_path_exists_at_all() {
        let content = load_output(None).unwrap();
        assert!(matches!(content, OutputContent::NoOutput));
    }

    #[test]
    fn truncated_capture_shows_the_marker_distinctly() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r1.out");
        std::fs::write(&path, format!("normal line{TRUNCATION_MARKER}")).unwrap();

        let content = load_output(Some(&path)).unwrap();
        let OutputContent::Loaded(loaded) = &content else {
            panic!("expected Loaded")
        };
        assert!(loaded.truncated);

        let backend = ratatui::backend::TestBackend::new(40, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_content(f, f.area(), &content, 0))
            .unwrap();
        let buf = terminal.backend().buffer();

        let marker_row = 1u16;
        let marker_style = buf[(0, marker_row)].style();
        let normal_style = buf[(0, 0)].style();
        assert_ne!(
            marker_style, normal_style,
            "the truncation marker must render with a style distinct from ordinary text"
        );
    }

    #[test]
    fn file_over_one_megabyte_loads_the_tail_and_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.out");
        let mut body = "x".repeat(usize::try_from(VIEW_TAIL_THRESHOLD).unwrap() + 500);
        body.push_str("TAIL-MARKER");
        std::fs::write(&path, &body).unwrap();

        let content = load_output(Some(&path)).unwrap();
        let OutputContent::Loaded(loaded) = &content else {
            panic!("expected Loaded")
        };
        assert!(
            loaded
                .label
                .as_deref()
                .is_some_and(|l| l.contains("last 1MB")),
            "got label: {:?}",
            loaded.label
        );
        assert!(
            loaded.text.ends_with("TAIL-MARKER"),
            "the tail, not the head, must be loaded"
        );
        assert!(
            (loaded.text.len() as u64) <= VIEW_TAIL_THRESHOLD + 100,
            "loaded {} bytes, expected roughly the tail threshold",
            loaded.text.len()
        );

        let text = render_to_text(&content, 60, 10);
        assert!(
            text.contains("showing last 1MB"),
            "the screen must say the file was truncated to a tail, not just              the struct; got: {text:?}"
        );
    }

    #[test]
    fn find_next_match_wraps_around_and_is_case_insensitive() {
        let lines = ["alpha", "BETA error", "gamma", "delta error"];
        assert_eq!(find_next_match(&lines, "error", 0), Some(1));
        assert_eq!(
            find_next_match(&lines, "error", 1),
            Some(3),
            "must move past the current line, not report it again"
        );
        assert_eq!(
            find_next_match(&lines, "error", 3),
            Some(1),
            "must wrap back to the top once past the last line"
        );
        assert_eq!(
            find_next_match(&lines, "ERROR", 0),
            Some(1),
            "must ignore case"
        );
    }

    #[test]
    fn find_next_match_is_none_when_the_query_is_empty_or_there_is_no_hit() {
        let lines = ["alpha", "beta"];
        assert_eq!(find_next_match(&lines, "", 0), None);
        assert_eq!(find_next_match(&lines, "nope", 0), None);
        assert_eq!(find_next_match(&[], "x", 0), None);
    }

    #[test]
    fn tui_output_view_shows_redacted_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r1.out");
        let marker = std::str::from_utf8(nightjar_config::redact::MARKER).unwrap();
        std::fs::write(&path, format!("connecting with password={marker}\n")).unwrap();

        let content = load_output(Some(&path)).unwrap();
        let text = render_to_text(&content, 60, 10);

        assert!(
            text.contains(marker),
            "the redaction marker on disk must reach the screen unchanged; got: {text:?}"
        );
    }

    #[test]
    fn running_run_tail_follows_its_output() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("live.out");
        std::fs::write(&path, "partial\n").unwrap();

        let first = load_output(Some(&path)).unwrap();
        assert_eq!(as_text(&first), "partial\n");

        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            writeln!(f, "more").unwrap();
        }

        let second = load_output(Some(&path)).unwrap();
        assert_eq!(
            as_text(&second),
            "partial\nmore\n",
            "each load must re-read the file rather than return a cached copy"
        );
    }
}
