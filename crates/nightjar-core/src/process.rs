use std::os::unix::process::CommandExt;
use std::process::Command;

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

fn with_sigxcpu_grace(soft: u64, inherited_hard: u64) -> u64 {
    soft.saturating_add(1).min(inherited_hard)
}

pub fn apply_limits(cmd: &mut Command, limits: &crate::limits::Limits) {
    let requested: Vec<_> = [
        (libc::RLIMIT_AS, limits.memory),
        (libc::RLIMIT_CPU, limits.cpu_time),
        (libc::RLIMIT_NPROC, limits.processes),
        (libc::RLIMIT_NOFILE, limits.files),
    ]
    .into_iter()
    .filter_map(|(resource, ceiling)| ceiling.map(|ceiling| (resource, ceiling)))
    .collect();

    if requested.is_empty() {
        return;
    }

    unsafe {
        cmd.pre_exec(move || {
            for (resource, ceiling) in &requested {
                let mut inherited = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(*resource, &raw mut inherited) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                let soft = (*ceiling).min(inherited.rlim_max);

                let hard = if *resource == libc::RLIMIT_CPU {
                    with_sigxcpu_grace(soft, inherited.rlim_max)
                } else {
                    soft
                };
                let to_apply = libc::rlimit {
                    rlim_cur: soft,
                    rlim_max: hard,
                };
                if libc::setrlimit(*resource, &raw const to_apply) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
}

const fn is_this_process_or_everything(pgid: libc::pid_t) -> bool {
    pgid <= 1
}

#[allow(clippy::similar_names)]
pub fn signal_group(pid: u32, signal: i32) {
    let Ok(pgid) = libc::pid_t::try_from(pid) else {
        return;
    };
    if is_this_process_or_everything(pgid) {
        return;
    }
    unsafe { libc::kill(-pgid, signal) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_hard_limit_sits_one_second_above_the_soft_one() {
        assert_eq!(with_sigxcpu_grace(600, u64::MAX), 601);
    }

    #[test]
    fn cpu_hard_limit_never_exceeds_what_this_process_inherited() {
        assert_eq!(
            with_sigxcpu_grace(600, 600),
            600,
            "raising a hard limit needs privilege the job does not have"
        );
        assert_eq!(with_sigxcpu_grace(u64::MAX, 900), 900);
    }

    #[test]
    fn a_group_id_that_would_reach_this_process_or_everything_is_refused() {
        for pgid in [-1, 0, 1] {
            assert!(is_this_process_or_everything(pgid), "pgid {pgid}");
        }
        assert!(!is_this_process_or_everything(2));
    }
}
