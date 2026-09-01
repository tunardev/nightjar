# nightjar-tui

The terminal UI behind `nightjar tui`: opens the same store the CLI and
daemon use, read-only, and polls it — there is no IPC to the daemon, so the
view keeps working even when the daemon is dead. Renders with `ratatui` and
handles its own raw-mode and panic-hook terminal restoration.

Depends on `nightjar-core`, `nightjar-config`, `nightjar-runner`, and
`nightjar-store`. Only `nightjar-cli` depends on it.

Central items: `cmd_tui`, `App`.
