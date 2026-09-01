# nightjar-store

SQLite-backed storage for run history: one row per job occurrence, written
before the job starts so a daemon that dies mid-run leaves a record instead
of a hole, plus the daemon's heartbeat and the migrations that keep the
schema current. `Store::open` retries through the narrow window where two
processes race to create the database file for the first time.

It depends on no other `nightjar-*` crate. `nightjar-daemon`,
`nightjar-runner`, `nightjar-tui`, `nightjar-web`, and `nightjar-cli` depend
on it.

Central types: `Store`, `Run` (`run`), `JobState` (`state`).
