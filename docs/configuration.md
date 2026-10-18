# Configuration

carte reads three kinds of TOML files:

| File | Holds |
| --- | --- |
| `~/.config/carte/config.toml` | Machine settings: TLD, ports, idle timeout, limits, sharing |
| `~/.config/carte/projects/<name>.toml` | One project: where its apps are, env and per-app settings |
| `carte.toml` in a repo | Defaults for that app, which can be committed |

The config directory is `~/.config/carte` and carte's state (logs, the
CA, pidfiles, instance data) lives in `~/.local/state/carte`, or under
`$XDG_CONFIG_HOME` and `$XDG_STATE_HOME` when set. `CARTE_CONFIG_DIR`
and `CARTE_STATE_HOME` set them directly; `carte setup` writes both into
the daemon's LaunchAgent.

Every key is optional unless noted. Unknown keys are errors, so a typo is
reported instead of ignored.

The daemon reloads files when they change. A file with an error keeps
its last working version, and `carte ls` and the terminal UI show the
error. Changes to `[share]` need a daemon restart.

**Durations** are strings of numbers with units `d`, `h`, `m` and `s`:
`"15m"`, `"1h30m"`. **Sizes** are a number of bytes, or a string with a
binary unit: `"512KB"`, `"256MB"`, `"1GB"`.

## Machine config

`~/.config/carte/config.toml`. A missing file means all defaults.

```toml
tld = "localhost"
http_port = 80
https_port = 443
dns_port = 9253
idle_timeout = "15m"

[limits]
memory_target = "50%"
hard_ceiling = 50

[share]
provider = "cloudflare"
domain = "dev.example.com"
```

| Key | Default | |
| --- | --- | --- |
| `tld` | `"localhost"` | The top-level domain apps are served under. Any other (like `"test"`) needs `/etc/resolver/<tld>`, which `carte setup` writes; apps answer under `.localhost` too |
| `http_port` | `80` | Plain HTTP |
| `https_port` | `443` | HTTPS |
| `dns_port` | `9253` | carte's DNS server, which `/etc/resolver/<tld>` points to for a `tld` other than `localhost` |
| `idle_timeout` | `"15m"` | How long an app runs without traffic before it's stopped; projects can override it |
| `teardown_delay` | `"10m"` | How long a stack's worktrees must stay gone before its [teardown](#teardown) runs; projects can override it |

Changing `tld` or the ports means running `carte setup` again;
`carte setup --tld <tld>` changes `tld` and does the rest.

### `[limits]`

Limits are targets, not walls: they decide which idle apps to stop and
never block a start.

| Key | Default | |
| --- | --- | --- |
| `memory_target` | `"50%"` | Above this, the least recently used apps are stopped. A percentage of RAM or a size (`"16GB"`) |
| `instance_target` | none | The same, for the number of running instances |
| `hard_ceiling` | `50` | No more instances than this start; requests get an error instead. Catches runaway loops |
| `pty_reserve` | `64` | Ptys left free for your own terminals; processes fall back to pipes rather than use them |
| `log_buffer` | `"1MB"` | Log kept per process |
| `log_budget` | `"256MB"` | Log kept in total; stopped apps' logs go first |

Apps that are pinned, handling requests, or kept running by an app that
depends on them are never stopped to meet a target.

### `[share]`

Set up by `carte setup --share <domain>`. Without this section, sharing
is off.

| Key | Default | |
| --- | --- | --- |
| `provider` | `"cloudflare"` | `cloudflare`, `ngrok`, or `none` to run your own tunnel to the share port |
| `domain` | required | The domain shared apps are served under. For ngrok, your static domain |
| `tunnel` | `"carte"` | The Cloudflare tunnel's name |
| `port` | `9254` | The local port the tunnel delivers requests to |

## Project files

`~/.config/carte/projects/<name>.toml`. The file name is the project's
name, and appears in hostnames: at least three characters, lowercase
letters, digits and dashes. Files in a subdirectory, like
`projects/clients/shop.toml`, appear in that folder in the terminal UI;
it doesn't change their names or hostnames.

```toml
root = "~/work"
include = "acme-*"
strip_prefix = "acme-"
requires = ["tcp://localhost:5432", "tcp://localhost:6379"]
idle_timeout = "30m"

[env]
"{{app.name | upper}}_URL" = "{{url:app}}"
DATABASE_URL = "postgres:///{{app.name | snake}}_{{checkout}}"

[apps.acme-frontend]
depends_on = ["billing", "accounts"]

[apps.acme-billing.processes.worker]
policy = "disabled"
```

A project of one app needs only its root:

```toml
root = "~/work/shop"
```

| Key | Default | |
| --- | --- | --- |
| `root` | none | The directory `include` entries and app paths are relative to. Without `include`, the root itself is the project's one app |
| `include` | the root | Where the apps are: a path or glob, or a list of them. See [Finding apps](#finding-apps) |
| `projects` | none | Makes the file a template for a project per directory. See [Templates](#templates) |
| `strip_prefix` | none | Removed from directory names to get the short names used in hostnames |
| `env_provider` | detected | Where the base environment comes from: `mise`, `direnv`, `shell` or `none`. See [Environment](#environment) |
| `env` | none | Env templates for every app. See [Env templates](#env-templates) |
| `requires` | none | Services that must be reachable before anything in the project starts: `tcp://host:port` or `unix:///path` |
| `scheme` | `"https"` | The scheme of URLs carte injects. `"http"` for apps that misbehave behind HTTPS |
| `idle_timeout` | machine's | Overrides the machine's idle timeout for this project |
| `teardown_delay` | machine's | Overrides the machine's teardown delay for this project |
| `apps` | none | Per-app settings, keyed by app name. See [Apps](#apps) |
| `processes` | none | Processes declared for many apps at once. See [Project-level processes](#project-level-processes) |

### Finding apps

`include` entries are paths and globs, relative to `root` unless they
start with `~` or `/`. Each directory they match is an app:

```toml
root = "~/work"
include = [
  "acme-*",                # separate repos
  "mono/apps/*",           # apps inside a monorepo
  "!mono/apps/docs",       # but not this one
  "sites/handbook",        # a directory outside git
]
```

- **Entries apply in order.** One starting with `!` leaves out what it
  matches, and a later entry can bring a match back. The last entry
  that matches a directory decides.
- **A glob only picks directories with something to run**: a
  `Procfile.dev`, `Procfile` or `config.ru`, or a `carte.toml` with
  processes. Libraries and other directories next to your apps are
  left out. A path without a glob is always an app. Globs match one
  directory level each; `**` isn't supported.
- **Matches are looked for in every checkout.** A pattern that reaches
  into a git repo is also matched in each of the repo's worktrees, so a
  stack has exactly the apps its worktree has: an app added on a branch
  exists only in that stack, and one deleted on a branch is gone from
  it. Asking for such an app where it doesn't exist says which stacks
  have it. Apps in other repos that have no worktree in a stack run
  from their main checkout there.
- **Directories outside git** are apps too, without worktrees. They run
  from their directory in every stack.
- **Linked worktrees** that a glob reaches are skipped: they belong to
  their repo's stacks.
- New directories and repos that match a glob join the project as they
  appear. An entry that matches nothing is reported.

A repo can name its own apps in the `carte.toml` at its root, so the
project file only needs to point at it. When `include` (or the implied
root) picks a repo whose `carte.toml` has `include` or `[apps.X]` tables
with a `path`, those are the repo's apps, relative to the repo, instead
of the repo itself:

```toml
# carte.toml at the root of ~/work/mono
include = ["apps/*", "!apps/docs"]

[apps.backend]
path = "services/api"
```

```toml
# ~/.config/carte/projects/mono.toml
root = "~/work/mono"
```

Each checkout's own `carte.toml` decides, so a branch that adds an app
to it has that app in its stack. In this file, `[apps.X]` tables only
take `path`; an app's settings go in its own directory's `carte.toml`.

An app that `include` finds is named after its directory. To give one a
different name, or to add an app `include` doesn't find, name it with
`path`:

```toml
[apps.billing]
path = "acme-billing"
```

The table's key is then the app's name, and the directory belongs to it
even when a glob also matches it. Renaming an app changes its hostnames
and its state directory; `carte prune` cleans up the old one.

### Templates

A project file with `projects` is a template: each directory its entries
match becomes a project of its own, named after the directory, with the
directory as its `root`. Every other key in the file applies to each of
them.

```toml
# ~/.config/carte/projects/clients/sites.toml
root = "~/clients/sites"
projects = ["*", "!archived-*"]
idle_timeout = "30m"

[apps.wiki]
aliases = ["old-wiki"]
```

With no `include`, each project's root is its one app, so
`~/clients/sites/wiki` is served at `wiki.localhost`. With `include =
"apps/*"`, each matched monorepo becomes a project with those apps.

- `projects` entries work like `include` entries: paths and globs
  relative to `root`, `!` to leave matches out, in order. A directory a
  glob matches only becomes a project when it has an app with something
  to run; a path without a glob always does. New directories join as
  they appear.
- `[apps.<name>]` tables apply in every project that has an app of
  that name.
- A project file with the same name as a generated project wins, and
  replaces it entirely.
- The generated projects appear in the template's folder in the
  terminal UI (`clients` above). The template's own file name only
  identifies it: `carte project edit sites` opens it, and
  `carte project ls` shows which projects came from it.
- Directory names that aren't valid project names, and names two
  templates both generate, are reported.

### Names and hostnames

An app's name is its directory name, the key of the `[apps.X]` table
that names it with `path`, or the project's name for the project's root.
Its short name is the name, with `strip_prefix` removed from directory
names. Hostnames are built from short names, project and stack:

| | Hostname |
| --- | --- |
| On main | `billing.acme.localhost` |
| In a stack | `billing.feature-x.acme.localhost` |
| An app whose short name is the project's | `acme.localhost`, `feature-x.acme.localhost` |
| Shared | `billing--feature-x--acme.<share domain>` |

Short names must be at least three characters and unique within the
project.

A stack's name comes from the folder names of its worktrees, not their
branches:

| Worktree of `acme-billing` | Stack |
| --- | --- |
| `~/worktrees/acme-billing/feature-x` | `feature-x` |
| `~/work/acme-billing/.worktrees/feature-x` | `feature-x` |
| `~/work/acme-billing-feature-x` | `acme-billing-feature-x` |
| `~/work/workspaces/checkout-flow/acme-billing` | `checkout-flow` |

- The folder name is lowercased, with anything outside `a-z`, `0-9` and
  `-` turned into a dash.
- A folder with the repo's own name gives way to the folder above it.
- Worktrees of different repos with the same name form one stack.
- Worktrees with a detached HEAD count like any other.
- Moving or renaming a worktree's folder (`git worktree move`) gives it
  a new stack name, and the old stack is
  [torn down](#teardown).

Problems are reported when two worktrees of a repo have the same name,
and when a worktree's name is `main` (it's skipped). A stack can't be
named like an app of its project either: the app wins, and the stack is
reported as unreachable.

## Apps

`[apps.<name>]` tables in a project file, or the top level of an app's
`carte.toml`. When both set a key, the project file wins, key by
key; `env` and `processes` are merged entry by entry. The project's
[`[processes]`](#project-level-processes) sit between the two.

`carte.toml` is read in each checkout, so a branch can change how its
app runs: in a worktree, that worktree's `env`, `env_files`,
`processes` and `replace_processes` apply. The other keys (`aliases`,
`subdomains`, `depends_on`, `policy`, `port`) shape the whole project,
so they come from the main checkout's file.

```toml
[apps.events]
path = "acme-events"
subdomains = true
aliases = ["acme-events"]
env_files = [".env"]
depends_on = ["accounts", "mail.other-project"]
policy = "pinned"

[apps.events.env]
TZ = "Europe/Oslo"
```

| Key | Default | |
| --- | --- | --- |
| `path` | none | Project files only: the app's directory, which names it after the table. See [Finding apps](#finding-apps) |
| `env` | none | Env templates for this app, over the project's |
| `env_files` | none | Files of `KEY=VALUE` lines, relative to the checkout, read in order |
| `depends_on` | none | Apps this app calls. See below |
| `aliases` | none | Extra hostnames, without the TLD, routed to the app on main. `carte import puma-dev` sets the old puma-dev names |
| `subdomains` | `false` | Pass extra labels on the left (`oslo.events.acme.localhost`) through to the app instead of returning 404 |
| `policy` | `"auto"` | Default policy for the app's processes. See [Policies](#policies) |
| `port` | none | Route to a fixed port instead of running processes, for an app you start yourself |
| `replace_processes` | `false` | Ignore the Procfile and run only the processes configured here |
| `processes` | none | Per-process settings. See [Processes](#processes) |

**`depends_on`** starts the listed apps together with this one, in the
same stack, and keeps them running while it runs. Name apps of the same
project by their full or short name. An app of another project is named by
its project, when that project has one app or an app named like the
project, or as `app.project`. Those always run on their project's main
stack.

## Processes

An app's processes come from the first of these that exists in its
checkout:

1. `Procfile.dev`
2. `Procfile`
3. `config.ru`, which runs `web: bundle exec puma -p $PORT`

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
| `ready` | `"port"` | When the app counts as started: `"port"` once the port accepts connections, or a path like `"/up"` once a GET of it answers without a 5xx |
| `scope` | detected | `instance` runs per stack; `checkout` runs once per checkout and is shared by every stack running from it |
| `pty` | `true` | Run under a pseudo-terminal. `false` uses pipes |
| `policy` | the app's | See [Policies](#policies) |
| `kind` | none | `"service"` or `"task"`, which start before the other processes, or `"teardown"`, which runs once a stack is removed. See [Services and tasks](#services-and-tasks) and [Teardown](#teardown) |
| `after` | none | Processes of the same kind this one starts after, like `["migrate"]` |
| `restart` | `"on-failure"`; `"never"` for tasks | `"on-failure"` restarts the process when it crashes after the start; `"never"` leaves it down. Not allowed on the HTTP process or a teardown. See [Crashes](#crashes) |
| `provides` | none | Env templates set on every process of the instance, like a server's socket URL. See [Services and tasks](#services-and-tasks) |

Every process gets its own `PORT`. A fixed port in the HTTP process's
command (`-p 3000`, `--port=3000`) is replaced with `$PORT`, with a
notice.

Processes that look like file watchers default to `scope = "checkout"`:
names containing `watch`, `css` and `js`, and commands with `-w`,
`--watch` or a `:watch` task. A process with `provides` always defaults
to `scope = "instance"`.

### Services and tasks

`kind` decides when a process starts. An instance starts its processes in
three tiers, each once the one before is done:

| Tier | `kind` | Done when |
| --- | --- | --- |
| 1 | `"service"` | Ready, as checked by `ready` |
| 2 | `"task"` | Exited with code 0 |
| 3 | none: everything else, from the Procfile or config | Ready |

```toml
[apps.acme-billing.processes.redis]
command = "redis-server --port $PORT --unixsocket $CARTE_RUN_DIR/redis.sock --save ''"
kind = "service"

[apps.acme-billing.processes.migrate]
command = "bin/rails db:prepare"
kind = "task"
```

- A service is ready once it accepts connections on `$PORT`, or, with
  `ready = "/path"`, once a GET of that path on `$PORT` answers without a
  5xx. A service must listen on `$PORT` even when the app talks to it
  through a socket: one that only listens on a unix socket never becomes
  ready, and fails the start at the boot timeout. Redis can do both, as
  above.
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
  [Environment](#environment).
- Names are plain variable names, not key templates.
- Only processes that run per instance provide env: `provides` on a
  process with `scope = "checkout"` is an error.
- `carte env` shows their source as `provided by redis`.
- Run consoles, rake tasks and other commands with
  [`carte exec`](cli.md#exec) so they get these values too, rather than
  `.env`'s.

### Crashes

Once an instance is ready, a process that crashes is restarted, unless
it has `restart = "never"`:

- A crash is a non-zero exit or death by a signal. A process that exits
  with code 0 is done: it shows as `exited` and isn't restarted.
- It's restarted on the same port with the same env, so sockets and
  URLs other processes use stay valid. carte waits 1s, 2s, 4s and 8s
  before the first four restarts; at the fifth crash within a minute it
  gives up, and the process stays down while the instance keeps running.
  `carte status` shows it as `gave up restarting`, `carte ls` as
  `down: worker (gave up)`, and the terminal UI marks it `gave up`.
  Starting it by hand (`carte up app/worker`) clears that and starts
  counting again.
- Restarts and the give-up show in `carte events`.
- Stopping it by hand, disabling it, stopping the instance or an idle
  stop never restart it, and cancel a restart that's waiting.
- During the start, nothing is restarted: a service or task that fails
  fails the start, as described in [Services and tasks](#services-and-tasks),
  and another process that crashes stays down.
- Tasks default to `restart = "never"`, and `"on-failure"` isn't allowed
  on them: they run once per start. The HTTP process takes no
  `restart`: its crash crashes the instance, and the next request starts
  it again.
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

### Project-level processes

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

### Teardown

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
  deleting a branch. The `main` stack is never removed, and a stack
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
  already ran aren't undone, but the folders are kept. carte only tears
  down stacks it saw with a worktree since the daemon started, and keeps
  pending teardowns in memory: a daemon restart forgets them.
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
  [directories](#instance-directories) and pidfiles. When one fails, the
  folders stay and the failure shows in `carte events`; it isn't run
  again. [`carte prune`](cli.md#prune) later lists such stacks, and
  removes their files without running any teardown.

Teardown commands are destructive by design, so make them specific to
the stack, with `{{stack}}` or `$CARTE_STACK`, and safe to run when
there's nothing to remove.

### Node apps

carte doesn't read `package.json`, so a Node app needs a `Procfile.dev`
(or a `command` in the project file). Every process gets its port in
`PORT`; dev servers that ignore it need it as a flag:

```
web: npx vite --port $PORT --strictPort
```

| Dev server | Command |
| --- | --- |
| Next.js | `next dev` reads `PORT` itself |
| Vite | `vite --port $PORT --strictPort` |
| Astro | `astro dev --port $PORT` |
| Anything else | Check that it reads `PORT`, or pass it as a flag |

A fixed port in the web command (`--port 5173`, `-p 3000`) is replaced
with `$PORT` automatically.

carte passes the `.test` hostname through to the app, and recent dev
servers refuse hostnames they don't know:

- **Vite** (and Astro, which runs on it) accepts them without config:
  carte sets `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS`, which Vite adds to
  `server.allowedHosts`. Vite 6.1 to 8.0 read it as a single hostname,
  so there it only works while it holds one domain: an app with aliases,
  or a machine with a share domain, needs `server.allowedHosts` set.
- **Next.js:** list the hostnames in `allowedDevOrigins` in
  `next.config.js`, as in `["*.acme.test"]`, so hot reloading works.

### Policies

| Policy | Starts with the app | Stops when idle |
| --- | --- | --- |
| `auto` | Yes | Yes |
| `pinned` | Yes | No, and keeps its app running |
| `disabled` | No; `carte up` starts it once | — |

`carte pin`, `carte disable` and the terminal UI change a policy until
the daemon restarts. With `--save` (or `S` in the terminal UI) the change
is written to the project file, keeping its comments and formatting.

## Environment

Each process's environment is built in layers, later ones winning:

1. **The provider.** The default is `mise` when mise is installed and
   the checkout has `mise.toml`, `.mise.toml` or `.tool-versions`;
   otherwise `shell`, the environment of a login shell in the checkout.
   `direnv` uses `direnv export json`; `none` uses only the daemon's own
   environment.
2. **Env files** from `env_files`.
3. **Templates:** the project's `[env]`, then the app's, then the
   process's.
4. **Provided values** from the `provides` of the instance's processes.
   See [Services and tasks](#services-and-tasks).
5. **Variables carte sets:**

| Variable | |
| --- | --- |
| `PORT` | The process's port |
| `CARTE_APP`, `CARTE_PROJECT` | App and project names |
| `CARTE_STACK` | `main`, or the stack's [name](#names-and-hostnames) |
| `CARTE_CHECKOUT` | `main`, or the stack name of the worktree the code runs from |
| `CARTE_BRANCH`, `CARTE_WORKTREE` | The checkout's branch (empty on a detached HEAD) and path |
| `CARTE_STATE_DIR` | The instance's [state directory](#instance-directories) |
| `CARTE_RUN_DIR` | The instance's [run directory](#instance-directories) |
| `CARTE_URL` | The app's URL in this stack |
| `CARTE_PUBLIC_URL` | Its public URL, while shared |
| `CARTE_TLD_LENGTH` | Labels after the app's own, for Rails' `tld_length` |
| `PIDFILE` | A pidfile path for the instance, in carte's state directory |
| `RAILS_DEVELOPMENT_HOSTS` | Every domain the instance answers under, for Rails' `config.hosts`: the project's, the stack's, the app's own with `subdomains = true`, its aliases on main, and the share domain |
| `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS` | The project's domain, aliases on main and the share domain, for Vite's `server.allowedHosts` |
| `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` | System roots plus carte's CA |
| `NODE_EXTRA_CA_CERTS` | carte's CA |
| `DISABLE_SPRING` | `1` |

When `mise.toml` or `.tool-versions` changes, running apps are marked
"env changed" rather than restarted. `carte env <app>` prints the result
with where each value came from.

### Instance directories

Each instance (an app in a stack) gets two directories of its own. carte
creates them before its processes start, and creates nothing inside them.

- The **state directory**, `CARTE_STATE_DIR`, is for data that should
  outlive a restart, such as a database's files. It's
  `<state>/data/<project>/<stack>/<app>` in carte's state directory
  (`~/.local/state/carte` by default).
- The **run directory**, `CARTE_RUN_DIR`, is for sockets and other
  files that don't need to survive a reboot. Its path is short, so a Unix
  socket in it stays under the operating system's limit of about 100
  bytes: a hash of project, stack and app in the per-user temp directory
  on macOS, in `$XDG_RUNTIME_DIR/carte` or `/tmp/carte-<uid>` on Linux.
  `CARTE_RUNTIME_HOME` in the daemon's environment moves it; `carte env`
  shows the daemon's path while it runs. The folder and each run
  directory must be real directories owned by you: carte refuses a
  symlink or another user's folder, and closes the folder to other
  users. Beside each run directory, a `<hash>.owner` file names the
  carte that made it, by its state directory's `data` folder.

Commands aren't templated, so they use the variables like `$PORT`. A
process that wants a subdirectory creates it:

```
redis: mkdir -p $CARTE_STATE_DIR/redis && exec redis-server --port $PORT --unixsocket $CARTE_RUN_DIR/redis.sock --dir $CARTE_STATE_DIR/redis
```

Templates use `{{state_dir}}` and `{{run_dir}}`. The process's
[`provides`](#services-and-tasks) gives the socket to the rest of the app:

```toml
[apps.acme-billing.processes.redis.provides]
REDIS_URL = "unix://{{run_dir}}/redis.sock"
```

The stack's folder, `<state>/data/<project>/<stack>`, also holds
`stack.toml`, where carte records each app's worktree, and the last
branch seen in them.

When a stack's worktrees are gone, its [teardown](#teardown) deletes
its directories. carte doesn't delete them when an app or project goes
away, when a teardown fails, or when the worktree went away while the
daemon wasn't running; [`carte prune`](cli.md#prune) removes those
leftovers.

### Env templates

Values in `env` tables can use `{{…}}` expressions:

```toml
[env]
DATABASE_URL = "postgres:///{{app.name | snake}}_{{checkout}}"
BILLING_URL = "{{url:billing}}"
```

| Variable | |
| --- | --- |
| `app.name`, `app.short` | The app's full and short name |
| `project` | The project's name |
| `stack`, `checkout`, `branch` | As in `CARTE_STACK`, `CARTE_CHECKOUT` and `CARTE_BRANCH` |
| `stack.id` | 8 lowercase letters and digits from a hash of project and stack name; the same for a recreated worktree and on every machine |
| `port` | The process's port |
| `state_dir`, `run_dir` | The instance's [directories](#instance-directories), as in `CARTE_STATE_DIR` and `CARTE_RUN_DIR` |
| `url:<app>` | Another app's URL in the same stack; `url:app` is the app itself |
| `public_url:<app>` | Another app's public URL; an error while it isn't shared |

Filters, applied in order:

| Filter | `acme-billing` becomes |
| --- | --- |
| `upper` | `ACME_BILLING` |
| `snake` | `acme_billing` |
| `slug` | `acme-billing`, lowercased with other characters turned into dashes |

A key with a template in it is a **key template**: it's rendered once per
app in the project, with `app` meaning each app in turn. This gives
every app a variable pointing at every other:

```toml
[env]
"{{app.short | upper}}_URL" = "{{url:app}}"
# BILLING_URL, FRONTEND_URL, ACCOUNTS_URL, …
```

A key template that uses `public_url:app` skips apps that aren't shared.
