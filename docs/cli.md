# Command line

Every command talks to the daemon over its Unix socket, except `env`,
`project`, `setup` and `import`, which also work without it. `carte` with
no command opens the terminal UI.

## Addresses

Commands that act on something take an address, `app@branch/process`,
where every part is optional and inferred from the current directory:

| Address | Means |
| --- | --- |
| *(none)* | The app whose checkout you're in, on its stack |
| `billing` | The app on main |
| `billing@feature-x` | The app in the `feature-x` stack |
| `@feature-x` | Every app in the stack |
| `billing/worker` | One process of the app |
| `acme` | A project: every stack of it |

Apps can be named by their full or short name, or any unambiguous part
of one. When an app has the same name as its project, the name means the
app; `--project acme` addresses the project itself, and resolves an
address inside it when given one.

## Commands

| Command | |
| --- | --- |
| `carte ls [-a]` | Running instances, with URL and notes; `--all` includes stopped ones |
| `carte status [address]` | URL, state, public URL, processes, holds, idle time and last error of an app |
| `carte up [address] [--no-wait]` | Start, and wait until ready |
| `carte restart [address] [--no-wait]` | Restart, and wait until ready |
| `carte down [address]` | Stop |
| `carte pin` / `unpin` `[address] [--save]` | Keep running through idle periods, or let it idle out again |
| `carte disable` / `enable` `[address] [--save]` | Never start, or allow again |
| `carte logs [address]` | Logs; see below |
| `carte events [address] [-f] [-n N]` | What carte did and why: starts, stops, crashes, reloads, shares |
| `carte env [address] [KEY] [-p process] [--values]` | The environment a process gets, with where each value comes from |
| `carte open [address]` | Open in the browser |
| `carte share [address] [--apps a,b] [--ttl 2h] [--auth] [--pin] [--replace]` | Put apps on the public internet |
| `carte unshare [address]` | End a share |
| `carte stop-all [--idle]` | Stop every instance, or the idle ones |
| `carte project add\|ls\|edit\|rm` | Manage project files |
| `carte setup [--dry-run\|--check\|--uninstall] [--share domain]` | Set up the machine |
| `carte import puma-dev` | Create projects from `~/.puma-dev` |
| `carte tui` | The terminal UI |
| `carte daemon` | Run the daemon in the foreground |

`pin`, `disable` and their opposites last until the daemon restarts;
`--save` writes them to the project file.

`up` and `restart` print `ready <app@stack>` per instance once it answers.
When one fails, they print why with its last log lines and exit with 5.
Restarting a single process waits for its app.

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

`carte env` shows values carte renders itself (its own variables and
templates) and hides values from the environment provider, the daemon's
environment and env files, which can hold secrets. `carte env KEY` shows
one variable with its value; `--values` shows them all.

## JSON output

Every command takes `--json`. Fields listed here are stable: they keep
their names and meaning, and new fields may be added. Enum values are
kebab-case strings. Times are milliseconds since the Unix epoch.

**`carte ls --json`** prints the daemon's snapshot:

```
{ projects: [{ name, folder, apps, stacks: [{ name, branch, instances: [Instance] }] }],
  problems, running_instances, memory_bytes, shares, tunnel }
```

**`carte status --json`** prints `{ instances: [Instance] }`.

An **Instance** is:

| Field | |
| --- | --- |
| `app`, `stack`, `checkout` | Names; `checkout` is `main` for an app running from its main checkout in another stack |
| `checkout_path` | Where its code is |
| `url`, `public_url` | `public_url` is null unless shared |
| `state` | `stopped`, `starting`, `ready`, `stopping` or `crashed` |
| `processes` | `[{ name, state, policy, temporary, pid, port, http, restarts, memory_bytes, cpu_percent }]`; `state` is `stopped`, `starting`, `running`, `stopping`, `exited` or `disabled`, and `policy` is `auto`, `pinned` or `disabled` |
| `holds` | What keeps it running: `{ kind: "pinned" \| "requests" \| "connections" \| "dependent", … }` |
| `idle_in_secs` | Until idle shutdown, when counting down |
| `last_error`, `last_event` | Null when there's none |
| `boot` | `{ elapsed_ms, waiting_for }` while starting |
| `env_changed` | Its tool versions changed since it started |
| `memory_bytes`, `cpu_percent` | Summed over its processes |

**`carte logs --json`** prints one line per log line:
`{ project, app, stack, process, at_ms, text, marker }`.

**`carte events --json`** prints one line per event:
`{ seq, at_ms, kind, message, subjects: [{ project, stack, app, process }], boot_ms, exit_code }`,
where `kind` is one of `started`, `ready`, `stopped`, `crashed`,
`restarted`, `config-reloaded`, `config-error`, `worktree-added`,
`worktree-removed`, `shared`, `unshared`, `share-expired`,
`tunnel-connected`, `tunnel-failed` and `policy-changed`.

**`carte env --json`** prints `{ instance, vars: [{ key, value, masked, source, overrides }] }`;
`value` is null when `masked` is true.

**`up`, `down`, `restart`, `pin` and the rest** print
`{ affected, messages, failures: [{ instance, title, detail, log }] }`.

## Exit codes

| Code | |
| --- | --- |
| 0 | Success |
| 1 | Any other error |
| 2 | Invalid arguments |
| 3 | The daemon isn't running |
| 4 | The address names nothing or more than one thing, there's no app in the current directory, or a process or env key doesn't exist |
| 5 | An app failed to start or didn't get ready in time |
