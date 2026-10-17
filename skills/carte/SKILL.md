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

Processes with `kind = "service"` (a Redis server, say) start and get ready
first and stop last; `kind = "task"` processes (such as
`bin/rails db:prepare`) run to completion before the rest start, on every
start of the app. A failing service or task fails the start, and its log is
in the error. A finished task shows as `done`.

Processes with `kind = "teardown"` never start with the app. When a
branch's worktree is removed (or the branch deleted) and stays gone for
`teardown_delay` (10 minutes by default), carte runs them once in the main
checkout with that stack's env, typically dropping the branch's database,
and then deletes the stack's data. They are destructive: removing a
worktree has consequences, and adding it back within the delay cancels
them. `carte events` shows `teardown-scheduled`, `teardown-finished` and
`teardown-failed`.

A service may also be declared in the project file for several apps at
once (`[processes.redis]`), so it isn't always in the repo's Procfile or
`carte.toml`. Its `provides` sets variables such as `REDIS_URL` on every
process of the app, even while the service is down; `carte env` shows them
as `provided by redis`. Use that value rather than `.env`'s.

## Run commands with the app's env

Run consoles, rake tasks, `rails runner`, migrations and scripts through
`carte exec`, so they get the app's env instead of `.env`'s and reach the
same Redis and services as its processes:

```sh
carte exec -- bin/rails runner 'puts Sidekiq::Queue.all.map(&:name)'
carte exec -- bin/rake db:migrate
carte exec -p worker -- bin/rails runner '…'   # one process's env, PORT included
```

If it refuses because a service isn't running, run the `carte up` it
suggests, then retry. Don't use `--no-check` unless the command doesn't
need the service.

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
`restart`, `down`, `logs`, `events`, `env` with hidden values, and `exec`
for commands you'd be allowed to run anyway.

Ask the user first before:

- `carte share`: it puts the app on the public internet. Only share when
  the user asks for it.
- `carte stop-all`, or stopping apps you're not working on
- `carte prune` without `--dry-run`: it deletes the data of stacks, apps
  and projects that are gone
- removing a worktree or deleting a branch in a project with teardown
  processes: after the delay, they destroy that stack's data
- `--save` on `pin`, `unpin`, `disable` or `enable`, which edits project
  files
- editing anything under `~/.config/carte`
- `carte env --values`, or `carte env <KEY>` for a value that may be a
  secret
