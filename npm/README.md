# nightjar

cron that tells you what happened.

This package is an `npx`/`npm` wrapper around the [`nightjar`](https://github.com/tunardev/nightjar)
Rust binary — it is **not** a JavaScript/TypeScript API. It resolves your
platform, downloads the matching prebuilt binary from GitHub Releases the
first time it runs, caches it locally, and then execs it with whatever
arguments you passed, passing stdout/stderr/exit code straight through.

## Usage

```sh
npx nightjar status
```

Supported platforms: macOS (arm64/x64) and Linux (arm64, x64 glibc, x64
musl) — the same five targets `nightjar`'s release workflow builds. Any
other platform/arch fails immediately with a clear error instead of
attempting a download.

## Local development: `NIGHTJAR_BIN`

Set `NIGHTJAR_BIN` to the path of a locally built binary to skip the
download/cache logic entirely and run that binary instead:

```sh
cargo build -p nightjar-cli
NIGHTJAR_BIN=$(pwd)/../target/debug/nightjar node bin/nightjar.js status
```

This is the only way to exercise this wrapper before a tagged GitHub
release exists.

## Current limitation

**No GitHub release has been tagged yet.** Until `v0.1.0` (matching this
package's version) is published with the release assets `nightjar`'s
workflow produces, installing and running this package without
`NIGHTJAR_BIN` set will fail at the download step. This isn't a bug — there
is nothing to download yet.

## License

MIT. See [LICENSE](../LICENSE).

