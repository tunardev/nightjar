# nightjar-daemon

The scheduling loop: for each job, computes the next occurrence, sleeps to
it, decides whether a wake finds it on time, overdue, or catching up a gap
the machine slept through, and hands the actual run to `nightjar-runner`.
Also owns the daemon's own lock file, heartbeat, and stop-signal handling so
`launchctl stop` / `systemctl --user stop` and a plain SIGINT both exit
cleanly.

Depends on `nightjar-core`, `nightjar-config`, `nightjar-runner`,
`nightjar-schedule`, and `nightjar-store`. Only `nightjar-cli` depends on
it.

Central type: `Daemon`.
