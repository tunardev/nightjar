# nightjar-remote

SSH fan-out for `nightjar --host`: runs `ssh <host> nightjar <command>
--json` against a list of hosts concurrently, capped at 8 in flight, and
collects each host's result — telling "unreachable" apart from "reachable
but no `nightjar` on `PATH`" apart from a normal exit. It owns none of the
merging or rendering of those results into a table; that stays with
whichever CLI command calls it.

Depends on `nightjar-core`. Only `nightjar-cli` depends on it.

Central items: `fan_out`, `HostResult` / `HostOutcome`.
