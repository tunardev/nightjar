use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

pub trait RawTerminal {
    fn enter(&mut self) -> io::Result<()>;
    fn leave(&mut self) -> io::Result<()>;
}

pub struct CrosstermRaw;

impl RawTerminal for CrosstermRaw {
    fn enter(&mut self) -> io::Result<()> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        Ok(())
    }

    fn leave(&mut self) -> io::Result<()> {
        crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen)?;
        crossterm::terminal::disable_raw_mode()?;
        Ok(())
    }
}

/// `Drop` restores on normal exit, including via `?`. Signals never
/// unwind, so they need a separate handler. The panic hook must restore
/// the terminal before its own panic message reaches the alt screen.
pub struct TerminalGuard<T: RawTerminal> {
    term: T,
    entered: bool,
}

impl<T: RawTerminal> TerminalGuard<T> {
    pub fn new(mut term: T) -> io::Result<Self> {
        term.enter()?;
        Ok(Self {
            term,
            entered: true,
        })
    }

    pub fn leave(&mut self) -> io::Result<()> {
        if !self.entered {
            return Ok(());
        }
        self.term.leave()?;
        self.entered = false;
        Ok(())
    }

    pub fn re_enter(&mut self) -> io::Result<()> {
        if self.entered {
            return Ok(());
        }
        self.term.enter()?;
        self.entered = true;
        Ok(())
    }
}

impl<T: RawTerminal> Drop for TerminalGuard<T> {
    fn drop(&mut self) {
        let _ = self.leave();
    }
}

/// Must run before the alt screen is entered (see `enter_tui`). A panic
/// before that would render into a screen already being torn down.
///
/// Restores unconditionally, unlike `TerminalGuard::leave`. There is no
/// way to ask a guard elsewhere on the stack what state it left behind.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::execute!(io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
        previous(info);
    }));
}

pub fn enter_tui<T: RawTerminal>(
    term: T,
    mut install_hook: impl FnMut(),
) -> io::Result<TerminalGuard<T>> {
    install_hook();
    TerminalGuard::new(term)
}

#[derive(Clone)]
pub struct SignalFlags {
    pub shutdown: Arc<AtomicBool>,
    pub resize: Arc<AtomicBool>,
}

/// A signal handler may only safely set a flag, so these just flip
/// atomics for the event loop to poll.
///
/// SIGKILL can't be caught by any process, so there is no handler for it.
pub fn install_tui_signal_handlers() -> io::Result<SignalFlags> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let resize = Arc::new(AtomicBool::new(false));
    for sig in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(sig, Arc::clone(&shutdown))?;
    }
    signal_hook::flag::register(signal_hook::consts::SIGWINCH, Arc::clone(&resize))?;
    Ok(SignalFlags { shutdown, resize })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct RecordingTerminal {
        log: Rc<RefCell<Vec<&'static str>>>,
    }

    impl RawTerminal for RecordingTerminal {
        fn enter(&mut self) -> io::Result<()> {
            self.log.borrow_mut().push("enter");
            Ok(())
        }
        fn leave(&mut self) -> io::Result<()> {
            self.log.borrow_mut().push("leave");
            Ok(())
        }
    }

    #[test]
    fn guard_restores_when_dropped() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let guard = TerminalGuard::new(RecordingTerminal {
            log: Rc::clone(&log),
        })
        .unwrap();
        assert_eq!(*log.borrow(), vec!["enter"], "construction must enter");

        drop(guard);
        assert_eq!(
            *log.borrow(),
            vec!["enter", "leave"],
            "drop must emit the inverse of what construction emitted"
        );
    }

    #[test]
    fn leave_then_re_enter_round_trips_for_the_suspend_path() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut guard = TerminalGuard::new(RecordingTerminal {
            log: Rc::clone(&log),
        })
        .unwrap();

        guard.leave().unwrap();
        guard.re_enter().unwrap();
        assert_eq!(*log.borrow(), vec!["enter", "leave", "enter"]);

        drop(guard);
        assert_eq!(*log.borrow(), vec!["enter", "leave", "enter", "leave"]);
    }

    #[test]
    fn second_leave_is_a_no_op() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut guard = TerminalGuard::new(RecordingTerminal {
            log: Rc::clone(&log),
        })
        .unwrap();

        guard.leave().unwrap();
        guard.leave().unwrap();
        assert_eq!(
            *log.borrow(),
            vec!["enter", "leave"],
            "a second leave must not restore twice"
        );
    }

    #[test]
    fn panic_hook_is_installed_before_the_alt_screen_is_entered() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let hook_log = Rc::clone(&log);

        let guard = enter_tui(
            RecordingTerminal {
                log: Rc::clone(&log),
            },
            move || hook_log.borrow_mut().push("install_hook"),
        )
        .unwrap();

        assert_eq!(
            *log.borrow(),
            vec!["install_hook", "enter"],
            "the hook must be installed before the terminal is entered"
        );
        drop(guard);
    }
}
