# nightjar-runner

Runs a job: spawns it in its own process group with resource limits and
output capture applied, redacts secrets from what it captures, dispatches
`on_failure` notifications and tracks failure-notification cooldowns, and
generates and installs the launchd/systemd service files behind `nightjar
service install`.

Depends on `nightjar-core`, `nightjar-config`, `nightjar-schedule`, and
`nightjar-store`. `nightjar-daemon`, `nightjar-tui`, and `nightjar-cli`
depend on it.

Central items: `execute` / `Outcome` (re-exported from `exec`), `Notifier`
(`notify`), `ServicePlan` (`service`).
