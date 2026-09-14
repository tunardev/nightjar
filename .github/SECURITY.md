# Security

## Reporting

Report a vulnerability through GitHub's
[private advisory form](https://github.com/tunardev/nightjar/security/advisories/new). Please do not
open a public issue for one.

Include what an attacker gains, the smallest reproduction you have, and the output of
`nightjar --version`. You will get an acknowledgement within a week.

## Supported versions

The latest release only. nightjar is pre-1.0, so fixes land on `main` and ship in the next tag.

## What is in scope

nightjar runs commands you wrote, with your privileges, on your machine. That is its job, not a
vulnerability. What follows is in scope:

- a secret from `[secrets]` reaching a capture file, the status page, a log line, or an alert
- the status page answering a request that did not come from this machine, or one without the token
- a job name or command escaping its quoting and being executed by the shell in a way the job file
  did not ask for
- a path in a job file reaching outside the nightjar home during retention or capture
- a run's output being attributed to the wrong run or the wrong job

## What is not

- binding the status page to a non-loopback address: nightjar refuses to, and tells you why
- a job doing anything a job can do, including deleting files or sending traffic
- the daemon running as the user who installed it
