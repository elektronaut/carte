# Command line

Every command talks to the daemon over its Unix socket, except `env`,
`exec`, `project`, `setup` and `import`, which also work without it. `carte` with
no command opens the terminal UI.

## Addresses

Commands that act on something take an address, `app@stack/process`,
where every part is optional and inferred from the current directory:

| Address | Means |
| --- | --- |
| *(none)* | The app whose checkout you're in, on its stack; in a repo's checkout outside its apps, such as a monorepo's root, every app of that stack |
| `billing` | The app on main |
| `billing@feature-x` | The app in the `feature-x` stack |
| `@feature-x` | Every app in the stack |
| `billing/worker` | One process of the app |
| `acme` | A project: every stack of it |

`up` and `restart` on a stack start the apps that belong to it: on
`@main` every app of the project, on another stack the apps with a
worktree in it and what they depend on. `up` refuses a whole project; name an app
or a stack.

In a [kube project](configuration.md#kube-projects), the app is a Service
and the stack a namespace:

| Address | Means |
| --- | --- |
| `grafana.prod` | Service `grafana` of kube project `prod`, in whichever namespace has it |
| `grafana.prod@monitoring` | The Service in namespace `monitoring` |
| `grafana.monitoring.prod` | The same, in hostname order |
| `--project prod grafana@monitoring` | The same |
| `prod@monitoring` | Every Service carte knows of in the namespace |
| `prod` | The whole kube project, unless an app has that name |

`up`, `down`, `restart`, `status`, `open`, `logs` and `events` work on
them; `up` and `logs` need a Service. `pin`, `disable`, `env`, `exec`
and `share` don't apply to Services.

Apps can be named by their full or short name, or any unambiguous part
of one. When an app has the same name as its project, the name means the
app; `--project acme` addresses the project itself, and resolves an
address inside it when given one.

## Commands

| Command | |
| --- | --- |
| `carte ls [-a]` | Running instances, with URL and notes, like processes that are down and when they restart; `--all` includes stopped ones |
| `carte status [address]` | URL, state, public URL, processes, holds, idle time and last error of an app; a crashed process shows when it restarts, and a process that serves [paths](configuration.md#paths) shows them |
| `carte up [address] [--no-wait]` | Start, and wait until ready |
| `carte restart [address] [--no-wait]` | Restart, and wait until ready |
| `carte down [address]` | Stop |
| `carte pin` / `unpin` `[address] [--save]` | Keep running through idle periods, or let it idle out again |
| `carte disable` / `enable` `[address] [--save]` | Never start, or allow again |
| `carte logs [address]` | Logs; see below |
| `carte events [address] [-f] [-n N]` | What carte did and why: starts, stops, crashes, reloads, shares, teardowns |
| `carte env [address] [KEY] [-p process] [--values]` | The environment a process gets, with where each value comes from |
| `carte exec [address] [-p process] [--no-check] -- command…` | Run a command, like a console or a rake task, with the app's env |
| `carte open [address]` | Open in the browser |
| `carte share [address] [--apps a,b] [--ttl 2h] [--auth] [--pin] [--replace]` | Put apps on the public internet |
| `carte unshare [address]` | End a share |
| `carte stop-all [--idle]` | Stop every instance and kube forward, or the idle ones |
| `carte prune [--dry-run] [-y]` | Remove leftover state folders, run directories and pidfiles; see below |
| `carte project add\|ls\|edit\|rm` | Manage project files |
| `carte setup [--dry-run\|--check\|--uninstall] [--tld tld] [--share domain]` | Set up the machine |
| `carte import puma-dev` | Create projects from `~/.puma-dev` |
| `carte import kube [--folder F] [--kubeconfig FILE]` | Create a [kube project](configuration.md#kube-projects) for each kubeconfig context you pick |
| `carte tui` | The terminal UI |
| `carte daemon` | Run the daemon in the foreground. It refuses to start while another daemon with the same state directory is running or still stopping |
| `carte daemon restart` | Restart the daemon launchd runs, after installing a new build or changing `[share]`. Running apps stop and start again on their next request |

`pin`, `disable` and their opposites last until the daemon restarts;
`--save` writes them to the project file.

`up` and `restart` print `ready <app@stack>` per instance once it answers.
When one fails, they print why with its last log lines and exit with 5.
Restarting a single process waits for its app.

### Prune

`carte prune` removes [instance directories](configuration.md#instance-directories)
and pidfiles that nothing that exists owns any more: those of stacks with
no worktree (never `main`), of apps no longer in their project, and of
projects whose file is gone. It lists them by project and stack with
their sizes, then asks before removing them. It only removes files: it
never runs a [teardown](configuration.md#teardown) or touches databases,
so it names the stacks whose teardown never ran or failed, whose external
resources may remain.

It leaves alone anything of a running instance, and every project whose
configuration or discovery has a problem (an `include` entry that matches
nothing, a failing `git worktree list`), and says so. Several cartes
(with different state directories) can share the run root, so a run
directory that no project names is only removed when the `.owner` file
beside it names this carte.

| Option | |
| --- | --- |
| `--dry-run` | Only list |
| `-y`, `--yes` | Remove without asking; needed when stdin isn't a terminal |

### Logs

| Option | |
| --- | --- |
| `-f`, `--follow` | Keep printing new lines |
| `-n N` | Lines from the buffer to show first (100) |
| `--since 5m` | Only lines from this long ago on |
| `--since-start` | Only lines from each process's latest run |
| `--search text` | Only lines containing the text |
| `--rank "words"` | The lines most relevant to the words, best first; `--top N` (20) |
| `--mark` | Add a marker with the time instead of showing logs |

### Env

`carte env` shows values carte renders itself (its own variables,
templates and processes' `provides`) and hides values from the environment provider, the daemon's
environment and env files, which can hold secrets. `carte env KEY` shows
one variable with its value; `--values` shows them all.

### Exec

Commands you run yourself in a checkout, such as a Rails console, read
`.env` rather than carte's env, so they can end up talking to another
Redis than the app's workers. `carte exec` runs them with the env the
app's processes get:

```sh
cd ~/work/acme-billing-feature-x
carte exec -- bin/rails console
carte exec billing@feature-x -- bin/rails runner 'ReportJob.perform_later'
```

- The env is what every process of the app shares: the provider, env
  files, the project's and app's templates, every process's `provides`
  and carte's variables, but no process's own `env` and no `PORT`.
  `-p worker` (or `billing/worker`) gives that process's env instead,
  with its `PORT` while it runs. Teardown processes have no env here:
  `-p` refuses them, and their `provides` aren't set.
- It's layered over your shell's environment, so `TERM`,
  `SSH_AUTH_SOCK` and the like stay. The daemon's own environment, which
  the app's processes start from, isn't part of it, nor are variables
  the `shell` provider's login shell passes on unchanged from it; your
  shell's values stand for those. What the provider sets or changes,
  such as mise's variables or a `PATH` your profile extends, still wins
  over your shell.
- The command runs in the current directory when that's inside the
  app's checkout, otherwise in the checkout's root.
- carte replaces itself with the command, so signals, the terminal and
  the exit code are the command's own.
- When a process that provides env (a Redis service, say) isn't
  running, `exec` refuses and suggests the `carte up` that starts it,
  exiting with 3 when the daemon is down and 5 otherwise. `--no-check`
  runs the command anyway. Apps without `provides` don't need to be
  running.
- It doesn't start the app. While the app runs, it doesn't idle out
  until the command ends: `carte status` shows it as `held by 1 exec`.
  Anything the command leaves running in the background keeps holding
  it until that exits too. A stop or `carte down` still stops it.

## JSON output

Every command takes `--json`. Fields listed here are stable: they keep
their names and meaning, and new fields may be added. Enum values are
kebab-case strings. Times are milliseconds since the Unix epoch.

**`carte ls --json`** prints the daemon's snapshot:

```
{ projects: [{ name, folder, apps, stacks: [{ name, branch, instances: [Instance] }] }],
  problems, running_instances, memory_bytes, shares, tunnel }
```

A project's `folder` is the path of its file's directory below
`projects/`, like `clients/sites`, and absent for a file at the top.

A stack's `name` comes from its worktrees' folder names
([names](configuration.md#names-and-hostnames)). Its `branch` is the
branch its worktrees have checked out, and null on `main`, when they are
on different branches, or when one has a detached HEAD.

**`carte status --json`** prints `{ instances: [Instance] }`.

An **Instance** is:

| Field | |
| --- | --- |
| `app`, `stack`, `checkout` | Names; `checkout` is `main` for an app running from its main checkout in another stack |
| `checkout_path` | Where its code is |
| `url`, `public_url` | `public_url` is null unless shared |
| `state` | `stopped`, `starting`, `ready`, `restarting` (its web process, or a process that serves [paths](configuration.md#paths), is waiting to restart after a crash or to pass its readiness check again), `stopping` or `crashed` |
| `processes` | `[{ name, state, policy, temporary, pid, port, http, paths, restarts, memory_bytes, cpu_percent, gave_up, restart_in_secs }]`; `paths` lists the [paths](configuration.md#paths) a process serves, left out when it serves none; `state` is `stopped`, `starting`, `running`, `stopping`, `exited`, `disabled` or `done` (a task that finished), `policy` is `auto`, `pinned` or `disabled`, `gave_up` is true once carte stopped restarting a process that kept crashing, and `restart_in_secs` is the wait before a crashed process restarts, left out when none is waiting |
| `holds` | What keeps it running: `{ kind: "pinned" \| "requests" \| "connections" \| "dependent" \| "exec", … }`; `exec` counts `carte exec` commands still running |
| `idle_in_secs` | Until idle shutdown, when counting down |
| `last_error`, `last_event` | Null when there's none |
| `boot` | `{ elapsed_ms, waiting_for }` while starting |
| `env_changed` | Its tool versions changed since it started |
| `warms` | `[{ app, hits, opens }]`: the apps opening it [warms](configuration.md#learned-warming), each followed `hits` of its last `opens` opens; left out when empty |
| `memory_bytes`, `cpu_percent` | Summed over its processes |

A `carte` older than the daemon it talks to shows a state, hold, policy or
event kind it doesn't know as `unknown`, in its output and in `--json`.
Restart the TUI or run the new `carte` to see what it is.

**`carte logs --json`** prints one line per log line:
`{ project, app, stack, process, at_ms, text, marker }`.

**`carte events --json`** prints one line per event:
`{ seq, at_ms, kind, message, subjects: [{ project, stack, app, process }], boot_ms, exit_code }`,
where `kind` is one of `started`, `ready`, `stopped`, `crashed`,
`gave-up`, `exited`, `restarted`, `config-reloaded`, `config-error`, `worktree-added`,
`worktree-removed`, `shared`, `unshared`, `share-expired`,
`tunnel-connected`, `tunnel-failed`, `policy-changed`,
`teardown-scheduled`, `teardown-paused`, `teardown-cancelled`, `teardown-finished` and
`teardown-failed`. A [teardown](configuration.md#teardown) has one
`teardown-finished` or `teardown-failed` event per process, about
`app@stack/process` and with its `exit_code`, and one about the stack
when its folders are removed or kept. A process that exits with code 0
while its instance runs on has an `exited` event, not `crashed`.

**`carte env --json`** prints `{ instance, vars: [{ key, value, masked, source, overrides }] }`;
`value` is null when `masked` is true.

**`up`, `down`, `restart`, `pin` and the rest** print
`{ affected, messages, failures: [{ instance, title, detail, log }] }`.

**`carte prune --json`** needs `--dry-run` or `--yes`, and prints
`{ list, result }`:

```
list:   { candidates: [{ kind, project, stack, app, path, bytes }], total_bytes,
          stacks: [{ project, stack }], skipped: [{ project, stack, reason }] }
result: { removed: [Candidate], removed_bytes, kept: [{ path, reason }],
          errors: [{ path, reason }] }
```

`kind` is `project`, `stack`, `app`, `run-dir` or `pidfile`; `project`,
`stack` and `app` are null when they don't apply, and a run directory
nothing names has no project. `stacks` are those whose external resources
may remain. `result` is null with `--dry-run` or when there's nothing to
remove; `kept` lists what stopped being a leftover or started running
before it was removed.

## Exit codes

| Code | |
| --- | --- |
| 0 | Success |
| 1 | Any other error |
| 2 | Invalid arguments |
| 3 | The daemon isn't running, or is starting or stopping |
| 4 | The address names nothing or more than one thing, there's no app in the current directory, or a process or env key doesn't exist |
| 5 | An app failed to start or didn't get ready in time, or `exec` found a process that provides env not running |
