use nightjar_cli::{require_utf8_args, run_cli};

fn main() {
    // Rust ignores SIGPIPE, so `nightjar status | head` would panic on EPIPE
    // instead of exiting quietly the way every other Unix tool does.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

    // `args_os()`, not `args()`: the latter panics on the first non-UTF-8
    // argument instead of letting us report it like any other bad input.
    let owned_args = match require_utf8_args(std::env::args_os().skip(1)) {
        Ok(args) => args,
        Err(e) => {
            eprintln!("nightjar: {e:#}");
            std::process::exit(1);
        }
    };
    let args: Vec<&str> = owned_args.iter().map(String::as_str).collect();

    match run_cli(&args) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("nightjar: {e:#}");
            std::process::exit(1);
        }
    }
}
