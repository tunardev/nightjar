# nightjar-schedule

Parses a job's `schedule` field into something the daemon can ask "when is
the next occurrence after this instant" — both cron expressions (5-field, or
6-field with a leading seconds column) and the human grammar (`hourly`,
`daily at 2am`, `weekdays at 9am`, `every 15 minutes`, `weekly sun at 3am`)
lower onto the same type, so nothing downstream needs to know which syntax a
job used.

It depends on no other `nightjar-*` crate. `nightjar-config`,
`nightjar-daemon`, `nightjar-runner`, and `nightjar-cli` depend on it.

Central type: `Schedule`.
