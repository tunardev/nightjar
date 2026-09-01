# nightjar-config

Job and global config parsing: TOML job files under `~/.config/nightjar/jobs`
plus the optional `config.toml`, validated on load — an unknown key or an
invalid combination (such as `cpu_time` with a shell that swallows
`SIGXCPU`) is a load error, not a silent partial parse. Also owns secrets:
resolving a job's `[secrets]` table through a configured resolver and
building the redaction set used to scrub captured output.

Depends on `nightjar-core` and `nightjar-schedule`. `nightjar-daemon`,
`nightjar-runner`, `nightjar-tui`, `nightjar-web`, and `nightjar-cli` depend
on it.

Central types: `Config`, `Job`.
