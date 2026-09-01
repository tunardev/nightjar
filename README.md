# nightjar

**Tell me what happened. Not "it ran." What happened.**

cron runs your jobs and then stays quiet. nightjar runs them, records every
one, and answers the questions you actually have come Monday: did it run, how
long did it take, did it fail, did it not run at all, and is it late right now.

```console
$ nightjar status
daemon not responding — last heartbeat 2m ago (pid 48105)
JOB              SCHEDULE             LAST RUN           EXIT     DURATION   NEXT
backup           daily at 2am         3m ago             ok       1.5s       in 6h
feed-fetch       every 1m             2m ago             ok       0.7s       OVERDUE 1m ago
metrics-push     every 1m             2m ago             MISSED   0.0s       OVERDUE 1m ago
sync-photos      hourly               3m ago             FAIL     0.3s       in 47m
```

cron's version of that screen is an empty mailbox.

## Your first job

```sh
nightjar add backup --cmd 'restic backup ~/Documents' --at 'daily at 2am'
nightjar service install
nightjar status
```

`service install` registers a launchd agent on macOS or a systemd user unit on
Linux. The scheduler starts at login, comes back after a reboot. Prefer to run
it yourself? `nightjar daemon` runs it in the foreground.

## A job is a TOML file

```toml
# ~/.config/nightjar/jobs/backup.toml
command = "restic backup ~/Documents"
schedule = "daily at 2am"
```

That's the whole file. `schedule` reads like English — `hourly`, `daily at
2am`, `weekdays at 9am`, `every 15 minutes`, `weekly sun at 3am` — or a
five-field cron expression like `*/15 * * * *`.

Everything else is optional:

```toml
command = "pg_dump app | gzip > /backups/app.sql.gz"
schedule = "every 6 hours"
timeout = "30m"
catchup = "once"
overlap = "skip"

[on_failure]
notify = true
webhook = "https://hooks.example.com/nightjar"
```

- `catchup` (`none` / `once` / `all`) — how much of a missed stretch to replay.
- `overlap` (`skip` / `queue` / `parallel`) — what happens when a run is still
  going at the next occurrence.
- `on_failure` — a desktop notification, a `webhook` POST, or a shell `run`.
- `[limits]` — hard ceilings on CPU, processes, files, memory. A breach is
  recorded `LIMIT`, not `FAIL`.
- `[secrets]` — pull values from a secret manager, put them in the job's env,
  redact them everywhere they'd show up again.

Files are strict. A typo'd `timout` refuses to load, it doesn't quietly drop
your timeout.

Already have a crontab? `nightjar import` turns it into job files, written
**disabled** so nothing runs twice while cron still owns them.

## Run one after another

```toml
# report.toml
command = "./make-report.sh"
after = ["backup"]
```

`report` runs each time `backup` **succeeds**. A failure, timeout, or limit
breach fires nothing. `schedule` and `after` are mutually exclusive — a job is
on the clock or waiting on a parent, never both.

## Skip nothing, say why

- **Sleep through the 2am backup and it doesn't vanish.** On wake, nightjar
  records every missed occurrence and re-runs the most recent one.
- **A run that didn't happen is `MISSED`, not a silence.** Late jobs read
  `OVERDUE` — which is also how you find out the scheduler died.
- **A hung `ssh` can't outlive its run.** `timeout` kills the whole process
  group, children included.
- **Byte-for-byte logs.** `nightjar logs backup` replays stdout and stderr
  exactly as they were; `--json` hands them to a script.

## Commands

```sh
nightjar add <name> --cmd '…' --at 'daily at 2am'   # write a job file
nightjar edit <job>                                  # $EDITOR, re-validate on save
nightjar rm <job>                                    # delete it
nightjar enable|disable <job>                        # keep the file, flip whether it fires
nightjar run <job>                                   # run now, print what it captured
nightjar list                                        # every job and its status
nightjar status [job]                                # last outcome, duration, what's due
nightjar logs <job> [-f]                             # captured output; -f tails a live run
nightjar daemon                                      # run the scheduler in the foreground
nightjar service install|uninstall|status            # run under launchd or systemd
nightjar import                                      # crontab -> job files (disabled)
nightjar doctor                                      # can your jobs actually run?
nightjar serve                                       # read-only status page over HTTP
nightjar tui                                         # browse history, drill into failures
```

`list`, `status`, `logs`, and `doctor` all take `--json`.

## Check many machines

```console
$ nightjar --host web1,web2 status
HOST         JOB              SCHEDULE             LAST RUN           STATUS     DURATION   NEXT
web1         backup           daily at 2am         3m ago             ok         1.5s       in 6h
web2         unreachable
```

fans `status`, `list`, or `logs` (without `--follow`) out to every host and
merges the answers into one table with a `HOST` column. It's plain `ssh` — no
agent, no new credentials. If `ssh web1 date` works, so does `nightjar --host
web1 status`. Up to 8 hosts at once, each with its own connect timeout, so one
dead machine can't stall the rest. An unreachable host is a row, not a crash.

It's **view-only, deliberately**. Remote control is refused, and it names the
exact ssh to run instead:

```console
$ nightjar --host web1 run backup
nightjar: remote control is not supported; run: ssh web1 nightjar run backup
```

## How is this different from…

**cron** — cron runs your jobs and forgets them. No exit codes, no durations,
no output, no record of the run that never happened. nightjar is a cron
replacement that keeps the history cron throws away.

**systemd timers, launchd** — first-class schedulers, but the job is your app
talking to the OS logging. You write the status reporting yourself, or you
don't have it. nightjar owns the run end to end and answers "what happened"
out of the box.

**workflow engines (dagu et al.)** — dependency graphs, retries, branches.
Great when you need them. nightjar deliberately refuses fan-in, cycles,
branches, and retries — it chains two jobs and gets out of the way.

## What works

macOS and Linux. Launchd and systemd. Your crontab imports. Any secret manager
you can shell out to. And a fleet, as long as you can already reach it over
`ssh`.

## Install

Build from source until the first release:

```sh
git clone https://github.com/tunardev/nightjar
cd nightjar && cargo install --path crates/nightjar-cli
```

At the first tagged release:

```sh
curl -fsSL https://nightjar.tunar.dev | sh
```

## License

MIT. See [LICENSE](LICENSE).
