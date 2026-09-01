use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_nightjar")
}

const ENTER_ALT_SCREEN: &[u8] = b"\x1b[?1049h";
const LEAVE_ALT_SCREEN: &[u8] = b"\x1b[?1049l";

struct ManagedChild(Box<dyn Child + Send + Sync>);

impl Drop for ManagedChild {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

fn spawn_tui(
    home: &std::path::Path,
    extra_env: &[(&str, &str)],
) -> (ManagedChild, Box<dyn MasterPty + Send>, Arc<Mutex<Vec<u8>>>) {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();

    let mut cmd = CommandBuilder::new(bin());
    cmd.arg("tui");
    cmd.env("NIGHTJAR_HOME", home);
    cmd.env("TERM", "xterm-256color");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }

    let child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);

    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut reader = pair.master.try_clone_reader().unwrap();
    let captured_writer = Arc::clone(&captured);
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => captured_writer.lock().unwrap().extend_from_slice(&buf[..n]),
            }
        }
    });

    (ManagedChild(child), pair.master, captured)
}

fn wait_exit(child: &mut ManagedChild, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if matches!(child.0.try_wait(), Ok(Some(_))) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_for_enter_alt_screen(captured: &Mutex<Vec<u8>>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if find(&captured.lock().unwrap(), ENTER_ALT_SCREEN).is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("nightjar tui never entered the alternate screen within 5s");
}

fn assert_alt_screen_was_left(captured: &Mutex<Vec<u8>>, context: &str) {
    let bytes = captured.lock().unwrap().clone();
    assert!(
        find(&bytes, LEAVE_ALT_SCREEN).is_some(),
        "{context}: the alt screen must actually be left, not only termios restored"
    );
}

#[test]
fn terminal_is_restored_when_quit_is_clean() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, master, captured) = spawn_tui(tmp.path(), &[]);

    let baseline = master.get_termios();
    wait_for_enter_alt_screen(&captured);

    let mut writer = master.take_writer().unwrap();
    writer.write_all(b"q").unwrap();
    drop(writer);

    assert!(
        wait_exit(&mut child, Duration::from_secs(10)),
        "nightjar tui did not exit after q"
    );

    let after = master.get_termios();
    assert_eq!(
        after, baseline,
        "termios must be back to how it was before the tui ever ran"
    );
    assert_alt_screen_was_left(&captured, "clean quit");
}

fn assert_restored_after_signal(sig: libc::c_int, label: &str) {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, master, captured) = spawn_tui(tmp.path(), &[]);

    let baseline = master.get_termios();
    wait_for_enter_alt_screen(&captured);

    let pid = child.0.process_id().expect("child has a pid");
    unsafe { libc::kill(libc::pid_t::try_from(pid).unwrap(), sig) };

    assert!(
        wait_exit(&mut child, Duration::from_secs(10)),
        "nightjar tui did not exit after {label}"
    );

    let after = master.get_termios();
    assert_eq!(
        after, baseline,
        "{label}: termios must be restored, not left in raw mode"
    );
    assert_alt_screen_was_left(&captured, label);
}

#[test]
fn terminal_is_restored_when_process_receives_sigterm() {
    assert_restored_after_signal(libc::SIGTERM, "SIGTERM");
}

#[test]
fn terminal_is_restored_when_process_receives_sigint() {
    assert_restored_after_signal(libc::SIGINT, "SIGINT");
}

#[test]
fn terminal_is_restored_when_process_receives_sighup() {
    assert_restored_after_signal(libc::SIGHUP, "SIGHUP");
}

#[test]
fn terminal_is_restored_and_message_is_readable_when_panic_occurs() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, master, captured) = spawn_tui(tmp.path(), &[("NIGHTJAR_TUI_TEST_PANIC", "1")]);

    let baseline = master.get_termios();
    wait_for_enter_alt_screen(&captured);

    assert!(
        wait_exit(&mut child, Duration::from_secs(10)),
        "nightjar tui did not exit after the injected panic"
    );
    std::thread::sleep(Duration::from_millis(200));

    let after = master.get_termios();
    assert_eq!(
        after, baseline,
        "termios must be restored after a panic, not left in raw mode"
    );
    assert_alt_screen_was_left(&captured, "panic");

    let bytes = captured.lock().unwrap().clone();
    let leave_at = find(&bytes, LEAVE_ALT_SCREEN).expect("checked by assert_alt_screen_was_left");
    let message_at = find(&bytes, b"nightjar tui: injected test panic");

    assert!(
        message_at.is_some(),
        "the panic message must reach the terminal"
    );
    assert!(
        leave_at < message_at.unwrap(),
        "panic message written before leaving alt screen"
    );
}

#[test]
fn tui_actually_enters_alternate_screen() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut child, master, captured) = spawn_tui(tmp.path(), &[]);
    wait_for_enter_alt_screen(&captured);

    let mut writer = master.take_writer().unwrap();
    writer.write_all(b"q").unwrap();
    drop(writer);
    wait_exit(&mut child, Duration::from_secs(10));
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
