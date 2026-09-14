# nightjar

cron that tells you what happened.

cron runs your job and forgets it. nightjar records every run: when it started, how long it
took, how it ended, and everything it printed. It answers the question cron never could:

```
$ nightjar logs sync-photos
sync-photos failed with exit 3 after 1.2s, 12m ago (run 019a4f2e-8c21-7b3a-9f01-0c2d4e6a8b10)
rsync: connection unexpectedly closed
```

## Install

```sh
curl -fsSL https://nightjar.tunar.dev | sh
```

## Start

```sh
nightjar add sync-photos --cmd 'rsync -a ~/Pictures backup:' --at 'every 15 minutes'
nightjar service install
nightjar status
```

`service install` registers the daemon with launchd or systemd so it survives logout and reboot.
`nightjar daemon` runs it in the foreground instead.

## Jobs

A job is one TOML file in `jobs/`. Only `command` and a schedule are required.

```toml
command  = "rsync -a ~/Pictures backup:"
schedule = "every 15 minutes"

timeout  = "5m"
catchup  = "once"
overlap  = "skip"
workdir  = "~/Pictures"

env      = { RSYNC_RSH = "ssh -o BatchMode=yes" }
secrets  = { BACKUP_TOKEN = "op://private/backup/token" }

[on_failure]
notify   = true
run      = "notify-send 'sync-photos failed'"

[limits]
memory   = "512MB"
cpu_time = "60s"
```

| field | what it does |
| --- | --- |
| `schedule` | `hourly`, `daily at 2am`, `weekly sun at 3am`, `every 15 minutes`, or a cron expression |
| `after` | run when another job succeeds, instead of on a clock |
| `timeout` | kill the run after this long, and record it as a timeout |
| `catchup` | what to do about firings missed while the daemon was down: `none`, `once`, `all` |
| `overlap` | what to do when the last run is still going: `skip`, `queue`, `parallel` |
| `secrets` | resolved at run time by the command in `config.toml`, and redacted from every capture |
| `limits` | address space, cpu time, process and file-descriptor caps, enforced by the kernel |

## Looking at what happened

```sh
nightjar status              # what each job did last, and when it runs next
nightjar logs sync-photos    # replay a run's output
nightjar logs sync-photos -f # follow one that is still going
nightjar tui                 # browse jobs, runs and output
nightjar serve               # the same, as a page on 127.0.0.1:18734
nightjar doctor              # check the machine over and name the fix for anything wrong
```

Every command takes `--json` and prints one versioned document, so scripts do not have to
parse a table.

`nightjar serve` binds loopback only. It will not bind anything else, with or without a token,
because an embedded HTTP server cannot cap what an unauthenticated peer costs it. The answer is an
SSH tunnel, which it tells you.

## Coming from cron

```sh
nightjar import          # turn `crontab -l` into job files, written disabled
nightjar import --enable # and enabled, once you have read them
```

Imported jobs are written disabled so nothing fires twice while cron still has them.

## Where things live

`NIGHTJAR_HOME` overrides everything; otherwise nightjar follows the XDG directories.

```
jobs/           one .toml per job
nightjar.db     every run, its outcome and its timings
runs/           captured stdout and stderr, pruned by retention
config.toml     defaults: retention, heartbeat, output cap, secrets resolver
```

## Building

```sh
make check   # fmt, imports, clippy, tests, cargo-deny
make test
make install
```

Clippy runs with `all`, `pedantic`, `nursery` and `cargo` enabled and nothing allowed away.

## License

[MIT](LICENSE).
