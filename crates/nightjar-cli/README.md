# nightjar-cli

The CLI implementation and the `nightjar` binary: argument parsing, one
module per subcommand (`add`, `run`, `status`, `logs`, `list`, `doctor`,
`import`, `service`, ...), and the `merged` / `notify` glue that turns
`--host` fan-out results into one rendered table. This is the integration
point for the whole workspace — it depends on every other crate here — so
its own logic is deliberately thin: wiring input to the right crate and
formatting what comes back.

Depends on every other crate in the workspace: `nightjar-config`,
`nightjar-core`, `nightjar-daemon`, `nightjar-remote`, `nightjar-runner`,
`nightjar-schedule`, `nightjar-store`, `nightjar-tui`, and `nightjar-web`.
Nothing else in the workspace depends on it.

```sh
cargo install --path crates/nightjar-cli
```

See the [root README](../../README.md) for actual usage — this crate is
where that documented behavior is implemented, not a second copy of it.
