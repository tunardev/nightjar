# nightjar-core

Shared primitives every other nightjar crate builds on: a `Clock` trait (with
`SystemClock` and a `FixedClock` for tests) so scheduling logic never calls
`Timestamp::now()` directly, XDG-aware path resolution, human-readable
formatting for timestamps and durations, and process-group handling —
spawning a job in its own process group, signaling the whole group, and
applying `RLIMIT_*` limits to it.

It depends on no other `nightjar-*` crate. Every crate except
`nightjar-schedule` and `nightjar-store` depends on it.

Central types: `Clock` / `SystemClock` (`clock`), `Paths` (`paths`), and
`Limits` (`limits`, re-exported from the crate root).
