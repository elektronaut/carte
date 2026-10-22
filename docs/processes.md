# Processes

An app's processes come from the first of these that exists in its
checkout:

1. `Procfile.dev`
2. `Procfile`
3. `config.ru`, which runs `web: bundle exec puma -p $PORT`
4. `package.json` with a `dev` script, which runs as `web` through the
   package manager. See [Node apps](#node-apps)

`[apps.<name>.processes.<process>]` tables change or add to that list.
With `replace_processes = true`, only the processes configured here run.

```toml
[apps.acme-billing.processes.web]
ready = "/up"

[apps.acme-billing.processes.worker]
policy = "disabled"

[apps.acme-billing.processes.mailcatcher]
command = "mailcatcher --foreground --http-port $PORT"
cwd = "tools"

[apps.acme-billing.processes.mailcatcher.env]
MAILCATCHER_SMTP = "1025"
```

| Key | Default | |
| --- | --- | --- |
| `command` | from the Procfile | The command, run with `sh -c`. Required for a process the Procfile doesn't have |
| `cwd` | the checkout | Working directory, relative to the checkout |
| `env` | none | Env templates for this process, over the app's |
| `http` | `true` for `web` | Whether this process serves the app's HTTP. At most one per app |
| `paths` | none | Paths of the app's hostname that go to this process instead of the HTTP process, like `["/vite-dev/"]`. See [Dev servers](dev-servers.md) |
| `preset` | detected | `"vite_ruby"` or `"shakapacker"` fills in `paths` and `provides` for that dev server; `"none"` turns off the detection. See [Presets](dev-servers.md#presets) |
| `ready` | `"port"` | When the app counts as started: `"port"` once the port accepts connections, or a path like `"/up"` once a GET of it answers without a 5xx |
| `scope` | detected | `instance` runs per stack; `checkout` runs once per checkout and is shared by every stack running from it |
| `pty` | `true` | Run under a pseudo-terminal, which [`carte attach`](cli.md#attach) connects to. `false` uses pipes |
| `policy` | the app's | See [Policies](#policies) |
| `kind` | none | `"service"` or `"task"`, which start before the other processes, or `"teardown"`, which runs once a stack is removed. See [Services and tasks](#services-and-tasks) and [Teardown](#teardown) |
| `after` | none | Processes of the same kind this one starts after, like `["migrate"]` |
| `restart` | `"on-failure"`; `"never"` for tasks | `"on-failure"` restarts the process when it crashes after the start; `"never"` leaves it down. Not allowed on the HTTP process or a process with `paths`, which are always restarted, or a teardown. See [Crashes](#crashes) |
| `provides` | none | Env templates set on every process of the instance, like a server's socket URL. See [Services and tasks](#services-and-tasks) |

Every process gets its own `PORT`. A fixed port in the command of the
HTTP process or a process with `paths` (`-p 3000`, `--port=3000`) is
replaced with `$PORT`, with a notice.

Stopping a process stops everything it started, including children it
moved to process groups of their own, as overmind or a job supervisor
does: each gets SIGTERM, then SIGKILL if it's still running after the
grace. The same happens when the process exits and leaves something
running.

On Linux this includes daemons a command starts on purpose, such as
`redis-server --daemonize yes` or `pg_ctl start`: they stop with the
process, so run such a service in the foreground as a process of its
own. On macOS, a child that starts a session of its own with `setsid`,
as a daemon does, is out of reach once its parent has exited, and
keeps running.

Processes that look like file watchers default to `scope = "checkout"`:
names containing `watch`, `css` and `js`, and commands with `-w`,
`--watch` or a `:watch` task. A process with `provides` always defaults
to `scope = "instance"`.

## Services and tasks

`kind` decides when a process starts. An instance starts its processes in
three tiers, each once the one before is done:

| Tier | `kind` | Done when |
| --- | --- | --- |
| 1 | `"service"` | Ready, as checked by `ready` |
| 2 | `"task"` | Exited with code 0 |
| 3 | none: everything else, from the Procfile or config | Ready; a process that serves no HTTP, such as a worker, once it has started |

```toml
[apps.acme-billing.processes.redis]
command = "redis-server --port $PORT --save ''"
kind = "service"

[apps.acme-billing.processes.migrate]
command = "bin/rails db:prepare"
kind = "task"
```

- A service that exits or isn't ready within the boot timeout, or a task
  that exits non-zero or doesn't finish within it, fails the start like a
  crashing web process: the later tiers don't start, and the error and
  the process's log show on the error page and in `carte up`.
- Requests wait for the whole sequence. Restarting a service or task by
  hand during it reruns it before the later tiers start; stopping one
  fails the start.
- Tasks run on every start of the instance, including after an idle
  stop, so they must be quick when there's nothing to do. A finished task
  shows as `done` and isn't run again until the next start; restarting
  one process doesn't run them.
- Stopping goes the other way: services get their signal once
  everything else has exited, so a job worker can hand its unfinished
  jobs back to Redis. The same holds when a crashed instance is started
  again: it starts once its old processes have exited.
- A disabled service or task is skipped, and nothing waits for it.
- After the start, a service that crashes is restarted; see
  [Crashes](#crashes).
- `after` orders processes within a tier: a task with
  `after = ["migrate"]` starts once `migrate` has finished, a process
  after `web` once `web` is ready.

The HTTP process can't have a `kind`, and services and tasks always run
per instance, never with `scope = "checkout"`.

A process with `kind = "teardown"` is in none of the tiers: it never
starts with the instance and doesn't show among its processes. See
[Teardown](#teardown).

A service tells the other processes where to find it with `provides`, a
table of env templates set on every process of its instance, itself
included:

```toml
[apps.acme-billing.processes.redis]
command = "mkdir -p $CARTE_STATE_DIR/redis && exec redis-server --port $PORT --unixsocket $CARTE_RUN_DIR/redis.sock --dir $CARTE_STATE_DIR/redis --save ''"
kind = "service"

[apps.acme-billing.processes.redis.provides]
REDIS_URL = "unix://{{run_dir}}/redis.sock"
```

- The values are set whenever the process is part of the app, even
  while it's disabled, stopped or crashed: a socket that isn't there
  fails loudly, where a missing `REDIS_URL` would quietly fall back to a
  shared Redis.
- They render in the receiving instance, like other templates, except
  that `{{port}}` is the providing process's port.
- They win over env files and every `[env]` template, a process's own
  included; only carte's own variables win over them. See
  [Environment](environment.md).
- Names are plain variable names, not key templates.
- Only processes that run per instance provide env: `provides` on a
  process with `scope = "checkout"` is an error.
- `carte env` shows their source as `provided by redis`.
- Run consoles, rake tasks and other commands with
  [`carte exec`](cli.md#exec) so they get these values too, rather than
  `.env`'s.

## Crashes

Once an instance is ready, a process that crashes is restarted, unless
it has `restart = "never"`:

- A crash is a non-zero exit or death by a signal. A process that exits
  with code 0 is done: it shows as `exited` and isn't restarted, and
  `carte events` records it as `exited`.
- It's restarted on the same port with the same env, so sockets and
  URLs other processes use stay valid. carte waits 1s, 2s, 4s and 8s
  before the first four restarts. While it waits, `carte status` shows
  `worker exited 1, restarting in 4s`, `carte ls` shows
  `down: worker (restarting in 4s)`, and the terminal UI counts down
  next to the process. At the fifth crash within a minute it gives up, and the process stays down while the instance keeps running.
  `carte status` shows it as `gave up restarting`, `carte ls` as
  `down: worker (gave up)`, and the terminal UI marks it `gave up`.
  Starting it by hand (`carte up app/worker`) clears that and starts
  counting again.
- Restarts show in `carte events`, and the give-up as `gave up`.
- A restart whose environment can't be built, such as when a
  `requires` entry is down or mise refuses the checkout, counts as a
  crash of that process: the error goes to its log, and the rest of the
  instance keeps running.
- Stopping it by hand, disabling it, stopping the instance or an idle
  stop never restart it, and cancel a restart that's waiting.
- During the start, nothing is restarted: a service or task that fails
  fails the start, as described in [Services and tasks](#services-and-tasks),
  and another process that crashes stays down.
- Tasks default to `restart = "never"`, and `"on-failure"` isn't allowed
  on them: they run once per start.
- The HTTP process takes no `restart`: it's restarted alone whenever it
  exits, with code 0 too, since nothing else answers on its port.
  Requests that arrive meanwhile wait until it's ready again, and the
  instance shows as `restarting` in `carte ls`, `carte status` and the
  terminal UI. A worker restarting on its own leaves the instance
  `ready`. A restarted HTTP process that isn't ready within the boot
  timeout counts as another crash: carte stops it, records the crash
  in `carte events`, notifies, and restarts it. carte also gives up
  after 5 such timeouts in a row without the process becoming ready in
  between, however far apart they are. When carte gives up on the HTTP
  process, the instance stops, and requests get the error page with
  its log instead of starting it, until you start it by hand
  (`carte up` or `carte restart`). Stopping it with `carte down` or
  `carte stop-all`, touching `tmp/restart.txt`, or a change to its
  configuration, Procfile or checkout clears that too, so the next
  request starts it as usual. `carte stop-all --idle` leaves it.
- A process with [`paths`](dev-servers.md) takes no `restart` either: it's
  restarted whenever it exits, like the HTTP process. Requests for its
  paths wait while it restarts, until it's ready again; other paths
  keep going to the HTTP process. Meanwhile the instance shows as
  `restarting`. A restart that isn't ready within the boot timeout
  counts as another crash, as for the HTTP process, and carte gives up
  after 5 such timeouts in a row. When carte gives up on it, only it stays down and
  the instance keeps running, and shows as `ready`, since pages still
  render. The process shows as `gave up` like any other, requests for
  its paths get a 502 with the error page and its log, and
  `carte restart app/vite` starts it again.
- A process with `scope = "checkout"` is restarted by the instance
  running it. Once it has exited, while it waits to restart or stays
  down, the other instances sharing the checkout don't start their own
  copy; when that instance stops, one of them takes it over.

Before restart policies, only services were restarted; other processes
stayed down after a crash. Set `restart = "never"` to keep that, for
one process or, in the project's `[processes]`, for every app's:

```toml
[processes.worker]
restart = "never"
```

## Notifications

carte shows a desktop notification when a process crashes after its
instance has started, when it gives up restarting one, and when a
process stops at a debugger prompt while no terminal is attached to it
("shop paused in web", with the [`carte attach`](cli.md#attach) command),
at most once a minute per process. Clean exits,
stops and failed starts, which show an error page, don't notify. After
a crash notification, further crashes of that instance within 5 minutes
come as one summary when the 5 minutes are up.

On macOS, carte uses `terminal-notifier` if it's installed
(`brew install terminal-notifier`), and clicking the notification opens
the app; otherwise it uses macOS's built-in notifications. On Linux it
uses `notify-send`. Set `notifications = false` in the
[machine config](configuration.md#machine-config) to turn them off; crashes while they're
off don't count towards a summary once they're back on.

## Project-level processes

`[processes.<process>]` tables in a project file declare a process for
several apps at once, with the same keys as
`[apps.<name>.processes.<process>]`, plus `apps`:

```toml
[processes.redis]
apps = ["billing", "acme-events"]
command = "mkdir -p $CARTE_STATE_DIR/redis && exec redis-server --port $PORT --unixsocket $CARTE_RUN_DIR/redis.sock --dir $CARTE_STATE_DIR/redis --save ''"
kind = "service"

[processes.redis.provides]
REDIS_URL = "unix://{{run_dir}}/redis.sock"

[apps.acme-events.processes.redis]
policy = "disabled"
```

| Key | Default | |
| --- | --- | --- |
| `apps` | every app | The apps that get the process, by full or short name. A name that isn't an app of the project is reported, and the others still get it |

Each listed app gets its own copy, as if it were in its
`[apps.<name>.processes]`: one Redis per app per stack, with its own
directories. When several places set the same process, later ones win,
key by key, with `env` and `provides` merged entry by entry:

1. The Procfile
2. The repo's `carte.toml`
3. The project's `[processes]`
4. The app's `[apps.<name>.processes]` in the project file

An entry without a `command` only adjusts the process in the apps that
already have it, from their Procfile, `carte.toml` or app table; the
other apps skip it silently. `[processes.worker]` with
`policy = "disabled"` turns off every app's worker, and apps without one
aren't affected.

`apps` is only valid here, not in an app's processes or `carte.toml`.

## Teardown

A process with `kind = "teardown"` cleans up after a stack once its
worktrees are gone, such as dropping the stack's database. It's
declared like any other process, in `[processes]`, an app's processes
or `carte.toml`, with the same env and templates:

```toml
[processes.drop-db]
apps = ["billing"]
kind = "teardown"
command = "dropdb --if-exists $DATABASE"

[processes.drop-db.env]
DATABASE = "billing_{{stack | snake}}"
```

- **When.** A stack counts as removed when no worktree has its name
  any more, and the directories of the worktrees carte last saw for it
  are gone. A rebase or a `git switch` inside the worktree keeps the
  directory, so it never counts, however long it takes; neither does
  deleting a branch. Nor does a worktree whose directory is missing
  while git still lists it, such as one on a volume that isn't mounted
  or a repo moved without `git worktree prune`; it counts once git no
  longer lists it. The `main` stack is never removed, and a stack
  where no instance ever ran has nothing to tear down. carte checks
  when its worktrees change and every 30 seconds.
- **Moving a worktree.** The stack is named after the worktree's
  folder, so moving or renaming the folder (`git worktree move`) to a
  different name removes the old stack: its teardown runs after
  `teardown_delay`, dropping its database or whatever else the
  teardown processes remove, while the checkout starts fresh under the
  new name. Move it back within the delay to keep the old stack.
- **Delay.** The stack must stay removed for `teardown_delay` (`"10m"`
  by default), so moving a worktree to a folder with the same name or
  adding it again doesn't tear anything down; a worktree with the name
  coming back cancels it. If
  it comes back while the teardown runs, the teardown processes that
  already ran aren't undone, the rest don't run, and the folders are
  kept; its apps start once the teardown has stopped. carte tears down
  stacks it saw with a worktree since the daemon started, and, when the
  daemon starts, stacks whose folders it finds with their worktrees gone:
  a teardown that was pending or cut short when the daemon stopped runs
  again after the delay, as does one for a worktree removed while it
  wasn't running.
- **Discovery.** Nothing is torn down in a project whose configuration
  or discovery has a problem, such as a failing `git worktree list` or
  an `include` entry that matches nothing, or in a stack whose
  `stack.toml` can't be read. A pending teardown then pauses
  (`teardown-paused` in `carte events`, with the problem): its delay
  keeps counting, but it only runs once carte can tell again that the
  stack is removed. Problems with a single worktree, such as one named
  `main` or two of a repo with the same name, don't pause the others.
- **Where.** Instances still running in the stack, such as apps without
  a worktree running from their main checkout, are stopped first. Then
  each app with a teardown process runs it once, in the app's main
  checkout (the worktree is gone; for an app that is only on branches,
  the main checkout of its repo), with the removed stack's environment:
  `{{stack}}`, `CARTE_STACK`, `{{state_dir}}`, `CARTE_STATE_DIR` and the
  rest are those of the app in that stack, `CARTE_BRANCH` is the last
  branch carte saw in the stack, and the provider runs in the main
  checkout. Services
  aren't running, so no `provides` are set. A process that doesn't
  finish within 2 minutes is stopped and counts as failed. `http` and
  `after` aren't allowed, and `scope` doesn't matter.
- **Afterwards.** Each run shows in `carte events` (`teardown-finished`
  or `teardown-failed`, with the exit code) and its output in
  `carte logs -f` and the daemon's log. When every teardown succeeds, or
  no app has one, carte deletes the stack's
  [directories](environment.md#instance-directories) and pidfiles. When one fails, the
  folders stay and the failure shows in `carte events`; it isn't run
  again until the daemon restarts. [`carte prune`](cli.md#prune) later
  lists such stacks, and removes their files without running any
  teardown.

Teardown commands are destructive by design, so make them specific to
the stack, with `{{stack}}` or `$CARTE_STACK`, and safe to run when
there's nothing to remove.

## Node apps

Without a Procfile or `config.ru`, a `package.json` with a `dev` script
runs as the `web` process: `pnpm run dev`, with the package manager
taken from the nearest lockfile in the checkout or above it (up to the
repo's root), then the `packageManager` field, then npm. carte doesn't
install packages; run `pnpm install` (or your package manager's
equivalent) in a new worktree yourself.

Every process gets its port in `PORT`. For dev servers that ignore it,
carte adds the flags they need, unless the script sets a port itself:

| The `dev` script starts | carte adds |
| --- | --- |
| `vite`, `react-router dev` | `--port $PORT --strictPort` |
| `astro dev`, `ng serve`, `webpack serve`, `rsbuild dev`, `vinxi dev` | `--port $PORT` |
| `next dev` | `-H 127.0.0.1`: it reads `PORT`, but otherwise listens on every network interface |
| Anything else (Nuxt reads `PORT`) | Nothing |

A script that runs more than one command, like `vite & tsc --watch` or
`vite && echo done`, gets nothing, since the flags would go to the last
command; carte says so. Put the flags in the script, or use a Procfile.

In a Procfile, pass the flags yourself (`web: pnpm exec vite --port
$PORT --strictPort`). A fixed port in the web command (`--port 5173`,
`-p 3000`) is replaced with `$PORT` automatically.

Every dev server above accepts `.localhost` hostnames as they are. For
another TLD, or a share domain:

- **Vite** and the frameworks built on it accept them: carte sets
  `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS`, which Vite adds to
  `server.allowedHosts`. Before 8.1, Vite reads it as one hostname, so
  carte passes only the project's domain (or the share domain, on
  `.localhost`); aliases and the rest need `server.allowedHosts`.
  Angular's dev server runs on Vite too.
- **Next.js** refuses its dev resources, hot reloading included, for
  origins outside `allowedDevOrigins`. carte drops the `Origin` header
  of same-origin requests for `/_next/` and `/__nextjs`, which Next
  lets through, so nothing needs configuring.
- **webpack-dev-server and Rsbuild** need the domain in their
  `allowedHosts`, like `[".acme.test"]`. When an app refuses a hostname,
  carte's error page says which setting to add it to.

The default `shell` env provider runs a login shell, so a Node version
from `.node-version` or `.nvmrc` needs a version manager that works
there: mise's shims (`mise activate --shims` in `.zprofile`), asdf or
Volta. `mise activate`, nvm and fnm set up in `.zshrc` only run in
interactive shells.

## Policies

| Policy | Starts with the app | Stops when idle |
| --- | --- | --- |
| `auto` | Yes | Yes |
| `pinned` | Yes | No, and keeps its app running |
| `disabled` | No; `carte up` starts it once | — |

`carte pin`, `carte disable` and the terminal UI change a policy until
the daemon restarts. With `--save` (or `S` in the terminal UI) the change
is written to the project file, keeping its comments and formatting. A
project made by a template has no file of its own, so `--save` refuses
rather than change every project of the template; give the project a
file of its own first.
