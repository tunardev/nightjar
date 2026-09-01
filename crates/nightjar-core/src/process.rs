use std::os::unix::process::CommandExt;
use std::process::Command;

/// `setsid`, unlike `setpgid`, also detaches the controlling terminal.
/// Abort on failure, or `signal_group` will hit the wrong group.
pub fn own_process_group(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Must stay async-signal-safe inside `pre_exec`, so this list is built
/// beforehand, not inside the closure.
pub fn apply_limits(cmd: &mut Command, limits: &crate::limits::Limits) {
    // Left to inference: `RLIMIT_*` is `c_int` on macOS, `u32` on glibc.
    // Naming either fails to compile on the other platform.
    let pairs: Vec<_> = [
        (libc::RLIMIT_AS, limits.memory),
        (libc::RLIMIT_CPU, limits.cpu_time),
        (libc::RLIMIT_NPROC, limits.processes),
        (libc::RLIMIT_NOFILE, limits.files),
    ]
    .into_iter()
    .filter_map(|(res, v)| v.map(|v| (res, v)))
    .collect();

    if pairs.is_empty() {
        return;
    }

    unsafe {
        cmd.pre_exec(move || {
            for (resource, value) in &pairs {
                // An unprivileged process can't raise its own hard limit (EPERM).
                let mut inherited = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(*resource, &raw mut inherited) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let effective = (*value).min(inherited.rlim_max);

                // `RLIMIT_CPU` only: soft breach raises SIGXCPU, hard breach
                // sends SIGKILL. Equal limits would send SIGKILL immediately,
                // same as this wrapper's timeout kill. The extra second keeps
                // them distinct.
                let hard = if *resource == libc::RLIMIT_CPU {
                    effective.saturating_add(1).min(inherited.rlim_max)
                } else {
                    effective
                };
                let rl = libc::rlimit {
                    rlim_cur: effective,
                    rlim_max: hard,
                };
                if libc::setrlimit(*resource, &raw const rl) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

/// Failure is almost always ESRCH: an already-empty group, not a real
/// error.
#[allow(clippy::similar_names)] // pid/pgid is standard POSIX terminology, not a typo risk
pub fn signal_group(pid: u32, sig: i32) {
    let Ok(pgid) = libc::pid_t::try_from(pid) else {
        return;
    };
    if pgid <= 1 {
        // kill(0, sig) hits the caller's own group. kill(-1, sig) broadcasts.
        return;
    }
    unsafe { libc::kill(-pgid, sig) };
}
