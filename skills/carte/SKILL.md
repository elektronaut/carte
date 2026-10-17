---
name: carte
description: Run, restart and debug web apps served by carte, the local daemon that runs each app's processes on demand behind https://<app>.test hostnames, one stack per git worktree. Use when working in a repo whose app is served by carte, when a `.test` URL or a `carte` command comes up, or before starting a dev server (bin/dev, rails s, npm run dev, foreman, overmind).
---

# carte

carte runs the app for you. Every app in a carte project has a URL like
`https://billing.acme.test`; the first request starts all of its processes
(web, workers, asset watchers) and carte stops them again when idle. Each
git worktree gets its own hostname, such as
`https://billing.feature-x.acme.test`.

## Don't start servers yourself

Never run `bin/dev`, `rails s`, `npm run dev`, `foreman` or `overmind` for an
app carte serves. They fight carte over ports and don't get carte's URLs or
environment. Use the app's URL, or `carte up`, instead.

## Find the app

```sh
carte status            # the app for the current directory
carte status --json     # url, state, processes, last error
carte ls                # everything that's running
```

Addresses look like `app@branch/process`; every part is optional and
inferred from the current directory. Run commands from inside the checkout
and you rarely need one. `--project <name>` addresses a whole project.

## Start and restart

```sh
carte up                  # start, and wait until it answers
carte restart             # restart, and wait until it answers
carte restart /worker     # one process
```

`up` and `restart` wait until the app is ready, and exit non-zero with the
error and the last log lines if it isn't. Restart after:

- changes the app doesn't reload by itself (initializers, gems, config)
- changes to `Procfile.dev`, `Procfile`, `mise.toml`, `.tool-versions` or
  env files; check the result with `carte env`

`touch tmp/restart.txt` also restarts the app.

## When something fails

```sh
carte logs --since-start              # this run's output, every process
carte logs --since 5m /worker         # one process, recent lines
carte logs --rank "undefined method"  # the most relevant lines, best first
carte events                          # starts, crashes, restarts, idle stops
carte env                             # the env the web process gets
carte env -p worker                   # another process's env
```

A request to an app that failed to boot gets an error page with the last
log lines. `carte events` answers "why did it stop or restart?".

`carte env` hides values that come from the environment and env files,
since they may be secrets. Don't reveal them without asking (see below).

## Parsing and exit codes

Every command takes `--json`. Exit codes:

| Code | Meaning |
| --- | --- |
| 0 | ok |
| 1 | other error |
| 2 | invalid arguments |
| 3 | the carte daemon isn't running |
| 4 | the address matches nothing, or there's no app in this directory |
| 5 | the app failed to start or didn't get ready |

On 3, tell the user; don't start the daemon or a server yourself.

## What you may do

Without asking, for the app you're working on: `carte status`, `ls`, `up`,
`restart`, `down`, `logs`, `events`, and `env` with hidden values.

Ask the user first before:

- `carte share`: it puts the app on the public internet. Only share when
  the user asks for it.
- `carte stop-all`, or stopping apps you're not working on
- `--save` on `pin`, `unpin`, `disable` or `enable`, which edits project
  files
- editing anything under `~/.config/carte`
- `carte env --values`, or `carte env <KEY>` for a value that may be a
  secret
