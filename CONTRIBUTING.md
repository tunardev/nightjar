# Contributing

Bug reports and patches are welcome. Read this first: nightjar has a few rules that are not the
usual ones, and the gates enforce them.

## Before you open a pull request

```sh
make check
```

That is fmt, import grouping, clippy, the whole test suite, and `cargo deny`, in that order. CI runs
the same thing, so a green `make check` is a green build.

`make fmt` needs the nightly rustfmt for import grouping. Everything else is stable.

## The rules that will surprise you

**No comments.** Not `//`, not `///`, not `/* */`, not `<!-- -->`, not `--` in SQL. The names and the
structure carry the meaning; if a line needs a comment, it needs a better name or a smaller function.
The two exceptions are the `# Errors` and `# Panics` sections clippy requires on public functions:
one short line each, naming the real failure, never restating the signature.

**Clippy allows nothing.** `all`, `pedantic`, `nursery` and `cargo` are on, with no lint set to
`allow`. If a lint fires, fix the code. On the rare occasion the lint is genuinely wrong, use
`#[expect(..., reason = "...")]` at the exact site so it errors once it stops applying, never a
blanket `allow`.

**Every message names the fix.** A user who sees an error should know what to do next. Lowercase, one
line, no trailing period, and the command they need in backticks. Anything a user reads more than
once lives in `nightjar-core::guidance` so it cannot drift between the CLI, the TUI and the web page.

**Tests go where they belong.** Unit tests in a `mod tests` at the end of their own file, always named
`tests`. Anything that drives the binary or crosses a crate boundary goes in `tests/`.

## Dependencies

`deny.toml` holds the licence allow-list. A new dependency needs a reason in the pull request and has
to clear `cargo deny check`. Prefer writing twenty lines to taking a crate for them.

## Commits

One line, conventional commits, no body:

```
fix(store): never prune a success whose after-children are still owed
```

Say what changed and why it matters, not which files moved.

## What nightjar will not do

These are decided, not open:

- **Fan-in.** `after` names one parent. A job that waits on several is a workflow engine; use one.
- **`@reboot`.** It is not a schedule. Start the job from the service that starts at boot.
- **Binding anywhere but loopback.** The embedded HTTP server cannot cap what an unauthenticated peer
  costs it, and a token does not change that. Reach it over an SSH tunnel.
- **A daemon that forgets.** Every run is recorded with its outcome. That is the whole point.

## License

By contributing you agree your work is published under the [MIT license](LICENSE).
