# nightjar-web

`nightjar serve`: a read-only, loopback-only HTTP view of job status — the
jobs page, a job's runs, a run's captured output, and the polling endpoint
the jobs page uses to refresh itself. Not part of the daemon; it only binds
a socket when a user runs `serve` by hand. Owns its own constant-time token
check and thread-per-request dispatch over `tiny_http`.

Depends on `nightjar-core`, `nightjar-config`, and `nightjar-store`. Only
`nightjar-cli` depends on it.

Central items: `serve`, `check_bind`, `DEFAULT_PORT`.
