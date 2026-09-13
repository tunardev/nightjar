use nightjar_cli::{require_utf8_args, run_cli};

fn main() {
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };

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
