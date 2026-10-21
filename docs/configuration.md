# Configuration

carte reads three kinds of TOML files:

| File | Holds |
| --- | --- |
| `~/.config/carte/config.toml` | Machine settings: TLD, ports, idle timeout, limits, sharing |
| `~/.config/carte/projects/<name>.toml` | One project: where its apps are, env and per-app settings; or a [kube project](#kube-projects) |
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
error. Changes to `[share]` need a daemon restart (`carte daemon restart`).

**Durations** are strings of numbers with units `d`, `h`, `m`, `s` and
`ms`: `"15m"`, `"1h30m"`, `"500ms"`. **Sizes** are a number of bytes, or a string with a
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

[attach]
detach_key = "ctrl-q d"
```

| Key | Default | |
| --- | --- | --- |
| `tld` | `"localhost"` | The top-level domain apps are served under. Any other (like `"test"`) needs `/etc/resolver/<tld>`, which `carte setup` writes; apps answer under `.localhost` too |
| `http_port` | `80` | Plain HTTP |
| `https_port` | `443` | HTTPS |
| `dns_port` | `9253` | carte's DNS server, which `/etc/resolver/<tld>` points to for a `tld` other than `localhost` |
| `idle_timeout` | `"15m"` | How long an app runs without traffic before it's stopped; projects can override it. An app with a process at a [debugger prompt](cli.md#attach) doesn't stop |
| `teardown_delay` | `"10m"` | How long a stack's worktrees must stay gone before its [teardown](#teardown) runs; projects can override it |
| `notifications` | `true` | Desktop [notifications](#notifications) when a process crashes, carte gives up on it, or it stops at a debugger prompt |

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

Apps that are starting, pinned, handling requests, running a `carte exec`
command, kept running by an app that depends on them, or waiting at a
[debugger prompt](cli.md#attach) are never stopped to meet a target.

### `[share]`

Set up by `carte setup --share <domain>`. Without this section, sharing
is off.

| Key | Default | |
| --- | --- | --- |
| `provider` | `"cloudflare"` | `cloudflare`, `ngrok`, or `none` to run your own tunnel to the share port |
| `domain` | required | The domain shared apps are served under. For ngrok, your static domain |
| `tunnel` | `"carte"` | The Cloudflare tunnel's name |
| `port` | `9254` | The local port the tunnel delivers requests to |

### `[attach]`

For [`carte attach`](cli.md#attach) and typing into a process from the
terminal UI.

| Key | Default | |
| --- | --- | --- |
| `detach_key` | `"ctrl-q d"` | The keys that detach: `ctrl-<key>` or single characters, separated by spaces. With two different keys, typing the first twice sends it once. One character alone isn't allowed. A letter after the prefix works on any keyboard layout, unlike `ctrl-]` or `~` |

## Project files

`~/.config/carte/projects/<name>.toml`. The file name is the project's
name, and appears in hostnames: at least three characters, lowercase
letters, digits and dashes. Files in a subdirectory, like
`projects/clients/shop.toml`, appear in that folder in the terminal UI;
it doesn't change their names or hostnames. Folders can nest:
`projects/clients/sites/nova.toml` appears in a `sites/` folder inside
`clients/`. Names must be unique across all folders, since they are
hostnames. Directories starting with a dot are skipped.

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
| `root` | none | The directory `include` entries and app paths are relative to, starting with `/` or `~/`. Without `include`, the root itself is the project's one app. Symlinks in it, and in absolute `include` entries and app paths, are resolved, so the apps are found from either path |
| `include` | the root | Where the apps are: a path or glob, or a list of them. See [Finding apps](#finding-apps) |
| `projects` | none | Makes the file a template for a project per directory. See [Templates](#templates) |
| `strip_prefix` | none | Removed from directory names to get the short names used in hostnames |
| `env_provider` | detected | Where the base environment comes from: `mise`, `direnv`, `shell` or `none`. See [Environment](#environment) |
| `env` | none | Env templates for every app. See [Env templates](#env-templates) |
| `requires` | none | Services that must be reachable before anything in the project starts: `tcp://host:port` or `unix:///path` |
| `scheme` | `"https"` | The scheme of URLs carte injects. `"http"` for apps that misbehave behind HTTPS |
| `idle_timeout` | machine's | Overrides the machine's idle timeout for this project |
| `teardown_delay` | machine's | Overrides the machine's teardown delay for this project |
| `serve_files` | none | The default [`serve_files`](#serving-files) of the project's apps |
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
  `Procfile.dev`, `Procfile` or `config.ru`, a `package.json` with a
  `dev` script, or a `carte.toml` with processes or `serve_files`. In a
  template, the project's `serve_files` folder counts too. Other directories next
  to your apps are left out; in a JS workspace, where libraries have
  `dev` scripts too, match the apps' folder (`apps/*`) or leave a
  library out with `!`. A path without a glob is always an app. Globs match one
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
  that name. One that matches no app in any of them is reported.
- A project file with the same name as a generated project wins, and
  replaces it entirely.
- The generated projects appear in the template's folder in the
  terminal UI (`clients` above, or a nested one like `clients/sites`
  for a template at `projects/clients/sites/static.toml`). The
  template's own file name only
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
- A name longer than 63 characters, the most a hostname label holds, is
  cut and ends in a short hash of the full name.
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
`subdomains`, `depends_on`, `policy`, `port`, `warm`) shape the whole
project, so they come from the main checkout's file.

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
| `env_files` | none | Files of `KEY=VALUE` lines, relative to the checkout, read in order. See [Env files](#env-files) |
| `depends_on` | none | Apps this app calls. See below |
| `aliases` | none | Extra hostnames routed to the app on main. An alias is one label without the TLD: `shop` serves `shop.localhost`. An alias equal to a project's name, or to an alias of a project loaded earlier, is skipped and reported, so an alias in a template applies to only one of its projects. `carte import puma-dev` sets the old puma-dev names. Rails apps: see `CARTE_TLD_LENGTH` under [Environment](#environment) |
| `subdomains` | `false` | Pass extra labels on the left (`oslo.events.acme.localhost`) through to the app instead of returning 404 |
| `policy` | `"auto"` | Default policy for the app's processes. See [Policies](#policies) |
| `port` | none | Route to a fixed port instead of running processes, for an app you start yourself |
| `serve_files` | none | A folder in the checkout, like `"public"`, whose files carte serves ahead of the app. See [Serving files](#serving-files) |
| `warm` | `true` | `false` keeps carte from starting the app ahead of a request. See [Learned warming](#learned-warming) |
| `replace_processes` | `false` | Ignore the Procfile and run only the processes configured here |
| `processes` | none | Per-process settings. See [Processes](#processes) |

**`depends_on`** starts the listed apps together with this one, in the
same stack, and keeps them running while it runs. Name apps of the same
project by their full or short name. An app of another project is named by
its project, when that project has one app or an app named like the
project, or as `app.project`. Those always run on their project's main
stack.

### Serving files

With `serve_files`, carte answers GET and HEAD requests for files in
that folder itself, and passes everything else on to the app, the way
puma-dev serves `public/`:

```toml
[apps.shop]
serve_files = "public"
```

- A path is looked up like Rails does: `/about` tries `about`,
  `about.html` and `about/index.html`, so `/` serves `index.html`.
- A file is served without starting the app, so assets and error pages
  load while it boots or after it crashes.
- An app with `serve_files` and nothing to run is a static site: carte
  serves its files, and a path with no file gets a 404.
- Each worktree serves the folder in its own checkout.
- Dotfiles (other than `.well-known`) and links that lead outside the
  folder are never served, also when the app is shared.

The folder is relative to the checkout; `"."` serves the checkout
itself. Set it at the top of a project file or template to give every
app in it the same folder.

### Learned warming

carte learns which apps you open after which, and starts them ahead of
time. An app is opened by its first request after it starts. When you
open `billing` within two minutes of `shop`, in the same stack, carte
notes that `billing` followed that open of `shop`. A request to
`billing` from a page of `shop` (by its `Origin` or `Referer`) counts
too, however long after.

carte keeps each app's last 10 opens. When `billing` followed at least
half of them, and at least 3, opening `shop` starts `billing` in the
same stack. What's learned on main applies to new worktrees right away.

A warmed app is an ordinary start: it idles out as usual and counts
toward the [limits](#limits). Nothing is warmed while memory or the
instance count is over its target. A warmed app that idles out without
a request counts against the guess, so guesses that don't help fade.
Nothing fades while you don't use an app.

`carte status shop` shows what it warms, as `billing (8/10)`, and
`carte events` says why a warmed app started. What carte has learned is
kept in `warming.json` in the state directory. `warm = false` keeps an
app from being warmed.

### Requests from other sites

A page on another site can't start an app. A request it makes for an
app that isn't running, such as an image, a `fetch` or a frame, gets a
403 page and leaves the app stopped. It doesn't open the app or warm
others. A request counts as another site's when its `Origin` or
`Referer` is a page outside carte's TLDs and loopback, its `Origin` is
hidden (`null`), or the browser marks it cross-site.

Opening the app yourself starts it, including by a link from another
site. So do requests from local pages and from clients like `curl`
that send no `Sec-Fetch-*` headers. Once the app runs, requests from
other sites reach it as usual; its own CORS and CSRF checks apply.

## Processes

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
| `paths` | none | Paths of the app's hostname that go to this process instead of the HTTP process, like `["/vite-dev/"]`. See [Paths](#paths) |
| `preset` | detected | `"vite_ruby"` or `"shakapacker"` fills in `paths` and `provides` for that dev server; `"none"` turns off the detection. See [Presets](#presets) |
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

### Services and tasks

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
  [Environment](#environment).
- Names are plain variable names, not key templates.
- Only processes that run per instance provide env: `provides` on a
  process with `scope = "checkout"` is an error.
- `carte env` shows their source as `provided by redis`.
- Run consoles, rake tasks and other commands with
  [`carte exec`](cli.md#exec) so they get these values too, rather than
  `.env`'s.

### Paths

A Rails app often runs a JavaScript dev server next to Rails, like
vite_ruby's `bin/vite dev` or Shakapacker's webpack-dev-server. Such a
process serves some paths of the app's hostname:

```toml
[apps.shop.processes.vite]
command = "bin/vite dev"
paths = ["/vite-dev/"]

[apps.shop.processes.vite.provides]
VITE_RUBY_PORT = "{{port}}"
```

- It gets a port of its own, like every process. carte sends requests
  under its paths to it, WebSocket upgrades included, and everything
  else to the HTTP process. The browser only talks to the app's
  hostname, so stacks don't collide on a fixed port, and shared apps get
  their assets too.
- A path takes itself and everything below it: `/vite-dev/` takes
  `/vite-dev` and `/vite-dev/@vite/client`, but not `/vite-development`.
  When paths of two processes match, the longer one wins. Two processes
  can't have the same path. Paths match the request's path as it
  arrives, before any decoding of `%` escapes or `..`.
- Its paths go to it ahead of [`serve_files`](#serving-files), so files
  an earlier build left in `public/vite-dev` aren't served instead.
- [`provides`](#services-and-tasks) tells Rails and the dev server the
  port, since `{{port}}` there is the dev server's port. It's set even
  while the dev server is down, so Rails never falls back to the
  default port.
- It counts toward the app's readiness like the HTTP process. It takes
  `ready`, `"port"` by default, and the first request waits for both
  under the boot timeout, so the first page doesn't render while its
  assets have nowhere to go. One that exits during the start or isn't
  ready in time fails the start, with its log on the error page.
- While it's disabled or stopped by hand, its paths go to the HTTP
  process. Rails then builds and serves the assets itself.
- It runs per instance, and takes no `kind`, `restart` or
  `scope = "checkout"`. The HTTP process can't have `paths`.

After the start it's always restarted, like the HTTP process; see
[Crashes](#crashes).

#### Presets

carte recognizes the two dev servers by their command, and fills in
`paths` and the port variable. Your own `paths` and `provides` win over
the preset's. Webpacker's older `bin/webpacker-dev-server` and
`bin/webpack-dev-server` aren't recognized.

| Command | `preset` | `paths` | `provides` |
| --- | --- | --- | --- |
| `bin/vite dev`, `bundle exec vite dev` | `"vite_ruby"` | `["/vite-dev/"]` | `VITE_RUBY_PORT = "{{port}}"` |
| `bin/shakapacker-dev-server` | `"shakapacker"` | `["/packs/", "/ws"]` | `SHAKAPACKER_DEV_SERVER_PORT = "{{port}}"` |

Set `preset` for a command carte doesn't recognize, and
`preset = "none"` to keep carte from detecting one. A detected preset
gives way when the process sets `kind`, `restart` or
`scope = "checkout"`, which a process with paths can't have: the process
runs as before, and `carte env` and `carte project edit` show a notice. Remove the setting and
set `preset` to have carte route its paths. Change `paths` when
the app has a different `publicOutputDir` (vite_ruby) or
`public_output_path` (Shakapacker).

Both dev servers read their port from the variable, and so does Rails,
which needs it to tell whether the dev server runs. Hot reloading needs
one change in the app:

- **vite_ruby** points Vite's hot-reload socket at the dev server's port
  on the page's hostname, which doesn't get through carte. Set
  `clientPort` to `0` in `vite.config.ts`: 0 tells Vite to use the port
  the client was loaded from, so the socket goes through carte:

  ```js
  export default defineConfig({
    server: { ws: { clientPort: 0 } }, // Vite 8.0 and older: server.hmr.clientPort
    plugins: [RubyPlugin()],
  })
  ```

  Outside carte, Vite then falls back to connecting to its own port, so
  `bin/dev` keeps working; the browser console shows one failed
  connection before the fallback.
- **Shakapacker** points webpack-dev-server's socket at
  `localhost:3035`. Point it at the page's host and port in
  `config/shakapacker.yml`, and carte passes `/ws` on:

  ```yaml
  development:
    dev_server:
      client:
        webSocketURL: "auto://0.0.0.0:0/ws"
  ```

  Outside carte, Rails doesn't pass the socket on, so hot reloading only
  works under carte with this setting.

vite_ruby records a running dev server in `tmp/vite-ruby-<mode>.json`,
and Rails trusts the record while its port matches. The dev server
removes it when it exits by itself, but not when it's killed, so when a
vite_ruby dev server exits, carte removes the record of its port too.

Vite accepts the app's other hostnames through
`__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS` (see [Node apps](#node-apps)).
webpack-dev-server refuses any hostname outside `.localhost` with a 403
"Invalid Host header", unless `allowed_hosts` in `config/shakapacker.yml`
lists it: add the project's domain for another TLD, and the share
domain for a shared app.

Settings that make the browser load assets from the dev server directly,
or over TLS, go around carte. Leave them off under carte:

- vite_ruby: `skipProxy: true`, `assetHost` (`VITE_RUBY_ASSET_HOST`) and
  `https: true`
- Vite: `server.ws.host` and `server.ws.protocol`
  (`server.hmr.host` and `server.hmr.protocol` before Vite 8.1)
- Shakapacker: `dev_server.server: https`, and `SHAKAPACKER_ASSET_HOST`

### Crashes

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
  `carte stop-all` clears that too, so the next request starts it as
  usual. `carte stop-all --idle` leaves it.
- A process with [`paths`](#paths) takes no `restart` either: it's
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

### Notifications

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
[machine config](#machine-config) to turn them off; crashes while they're
off don't count towards a summary once they're back on.

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
  [directories](#instance-directories) and pidfiles. When one fails, the
  folders stay and the failure shows in `carte events`; it isn't run
  again until the daemon restarts. [`carte prune`](cli.md#prune) later
  lists such stacks, and removes their files without running any
  teardown.

Teardown commands are destructive by design, so make them specific to
the stack, with `{{stack}}` or `$CARTE_STACK`, and safe to run when
there's nothing to remove.

### Node apps

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

### Policies

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

## Environment

Each process's environment is built in layers, later ones winning:

1. **The provider.** The default is `mise` when mise is installed and
   mise has config for the app: `mise.toml`, `.tool-versions` or another
   of mise's config files, such as `mise.local.toml` or
   `.config/mise/config.toml`, in the app's directory or one above it up
   to the repo's root (outside a repo, only the app's own directory; never
   your home directory's global config);
   otherwise `shell`, the environment of a login shell in the checkout.
   When that shell's `PATH` has mise's shims, carte adds `mise env` on
   top, since a shim would otherwise apply mise's `[env]` over carte's
   values inside every process. `direnv` uses `direnv export json`; `none` uses only the daemon's own
   environment. The provider has 30 seconds to finish. After that carte
   stops it and every process it started, and the app fails to start, so
   let a slow first run, such as `use flake` in `.envrc`, finish in a
   shell in the checkout first.
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
| `CARTE_TLD_LENGTH` | Labels after the app's own, for Rails' `tld_length`. It counts from the app's own hostname, so it doesn't fit an alias with fewer labels. With `subdomains = true`, `oslo.frontend.nova.localhost` gives Rails' `request.subdomain` `oslo`, but the alias `oslo.nova-frontend.localhost` gives an empty one |
| `PIDFILE` | A pidfile path for the instance, in carte's state directory |
| `RAILS_DEVELOPMENT_HOSTS` | Every domain the instance answers under, for Rails' `config.hosts`: the project's, the stack's, the app's own with `subdomains = true`, its aliases on main, and the share domain |
| `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS` | The project's domain, aliases on main and the share domain, for Vite's `server.allowedHosts` |
| `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` | System roots plus carte's CA |
| `NODE_EXTRA_CA_CERTS` | carte's CA |
| `DISABLE_SPRING` | `1` |

When a mise config file (`mise.toml`, `mise.local.toml`, `.tool-versions`
and the like) or a version file (`.node-version`, `.nvmrc`,
`.ruby-version`, `.python-version`) changes in an app's checkout or its
repo's root, running apps are marked "env changed" rather than
restarted. Changes in other directories, such as `.config/mise/` or a
folder between the repo's root and the app, aren't watched: restart the
app after them. `carte env <app>` prints the result
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
away or when a teardown fails; [`carte prune`](cli.md#prune) removes
those leftovers. It removes a run directory only when its `.owner` file
names this carte, and deletes the `.carte-prune-*` folders an
interrupted prune left.

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
| `host:<app>` | The hostname of `url:<app>`, without scheme or port: `billing.feature-x.acme.localhost` |
| `public_host:<app>` | The hostname of `public_url:<app>`; an error while it isn't shared |

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

A key template that uses `public_url:app` or `public_host:app`, in its
key or its value, skips apps that aren't shared.

Variable names are letters, digits, underscores and dots (for Spring's
`server.port` style), not starting with a digit or dot; a name that
isn't, written or rendered, is an error. For a literal
`{{`, write `{{"{{"}}`, in a single-quoted TOML string: `DOCKER_FORMAT = '{{"{{"}}.Names}}'` gives
`{{.Names}}`.

### Env files

Each line of an env file is `KEY=VALUE`, optionally after `export `.
Blank lines and lines starting with `#` are skipped.

- An unquoted value runs to the end of the line, or to ` #`, which starts
  a comment. Surrounding spaces are dropped.
- A single-quoted value is taken as is.
- A double-quoted value can span lines, and understands `\n`, `\t`,
  `\r`, `\"`, `\\` and `\$`.
- `${VAR}` and `$VAR` are kept as written, not expanded.

## Kube projects

A project file with a `kube` table is a kube project: its apps are the
Services of a Kubernetes cluster, and its stacks are the cluster's
namespaces. carte forwards a request for one to a pod behind the Service,
the way `kubectl port-forward` would, without anything to start by hand.

```toml
# ~/.config/carte/projects/prod.toml
kube = { context = "arn:aws:eks:eu-west-1:123456:cluster/production" }
namespaces = ["monitoring", "logging"]

[apps.grafana]
aliases = ["grafana-prod"]

[apps.vlogs]
service_port = "http"
```

| Host | Goes to |
| --- | --- |
| `grafana.monitoring.prod.localhost` | Service `grafana` in namespace `monitoring` |
| `grafana.prod.localhost` | The namespace that has a Service `grafana`; when several do, an error page lists them |
| `grafana-prod.localhost` | An alias of the Service |

With `namespaces` set, `grafana.prod.localhost` skips the namespaces the
cluster doesn't let you read, and fails only when it reads none of them.
When it doesn't find the Service, the page names the namespaces it
couldn't read.

A cluster is only reachable when it has a project file, so nothing about
your kubeconfig is exposed until you add one. `carte import kube` offers
a project for each context of your kubeconfig, named after its cluster:
`arn:aws:eks:…:cluster/production` becomes `production`, and
`gke_acme-infra_europe-north1_staging` becomes `staging`. It asks for a
name when that one is taken, and `--folder clusters` puts the files in
a folder of their own. It reads `--kubeconfig`, else the file in
`$KUBECONFIG`, else `~/.kube/config`, and names a file other than the
default in the projects, since the daemon doesn't see your shell's
`$KUBECONFIG`. Kube projects are never
reachable through [sharing](sharing.md).

| Key | Default | |
| --- | --- | --- |
| `kube.context` | required | The kubeconfig context |
| `kube.kubeconfig` | `$KUBECONFIG`, then `~/.kube/config` | The kubeconfig file, starting with `/` or `~/` |
| `namespaces` | all | The namespaces carte looks in |
| `env_provider` | detected | `mise`, `direnv`, `shell` or `none`: the env kubeconfig login plugins like `aws` or `gke-gcloud-auth-plugin` run with. Detected as for apps, in your home directory. See [Environment](#environment) |
| `idle_timeout` | machine's | How long a Service stays connected without requests |

`[apps.<service>]` tables add settings to a Service the cluster has; they
don't declare one. Every Service can be reached without a table.

| Key | Default | |
| --- | --- | --- |
| `aliases` | none | Extra hostnames, as for [apps](#apps) |
| `namespace` | looked up | The namespace `service.project` and the aliases mean; one of `namespaces` when that's set |
| `service_port` | see below | The Service's port, by name or number, for a Service with several |
| `policy` | `"auto"` | `"disabled"` refuses the hostname |

Without `service_port`, carte takes the Service's only port, else one
named `http`, `web` or starting with `http-`, else port 80, 8080 or 443.
Otherwise the error page lists the ports.

The keys about apps on disk (`root`, `include`, `env`, `processes` and
the like) don't apply to a kube project and are errors.

### Forwarding

carte looks up the Service when a request comes in and picks a ready pod
behind it. Every connection the proxy opens to it gets its own
port-forward stream, so a forward survives the laptop sleeping, a network
change and an expired token: the next connection simply opens a new
stream. There's no local port to reach a forward by; only requests
through carte's proxy get there. When the pod goes away, carte picks
another ready one. A forward nobody used for `idle_timeout` is closed and
opens again on the next request.

- **Kubeconfig changes**: carte fetches the provider env and reads the
  kubeconfig again when one of its files changes: `kube.kubeconfig`,
  else each file in `KUBECONFIG` of the provider env, else
  `~/.kube/config`, or when a file the context names by path changes:
  `certificate-authority`, `client-certificate` or `client-key`. It
  connects again only when what the project's context resolves to
  changed: the context, its cluster or its user, the contents of those
  files, and for a user with a login plugin, the env the plugin runs
  with. Switching `current-context` with `kubectx` or `kubectl config
  use-context` changes nothing. While the kubeconfig can't be read or
  parsed, or lacks the context, its cluster or its user, as in the
  middle of a rewrite, carte keeps the connection it has, logs a
  warning, and reads the files again on the next request. It does the
  same while a file the context names can't be read or connecting with
  it fails, as when a certificate was replaced but its key not yet, as
  long as the context, its cluster and its user are otherwise the same.
  When they changed and connecting fails, carte reports the error. A
  file counts as changed when its modification time, size or inode
  changes. After a change, and after a change to the project file,
  new connections use the new settings while open ones, websockets
  included, run until they close. Open connections end only with `carte
  down`, `carte restart` or `carte stop-all`, the idle timeout, setting
  the Service to `disabled`, the pod going away, or the daemon stopping.
  Idle connections the proxy keeps are closed after 30 seconds.

- **Login plugins**, like `aws eks get-token`, run with the env the
  project's env provider gives in your home directory, so they find the
  tools and profiles your shell has. When one fails, the error page shows its
  output, with a hint for `aws`, `gcloud` and `kubelogin`.
- **A cluster that doesn't answer**, like after the laptop sleeps or a
  VPN drops, gets 30 seconds for each call, login and stream it opens.
  Then the request gets a 504 page, "The cluster didn't answer", and
  the Service listing and `carte logs` fail with the same message.
  Streams already open, like a followed log or a WebSocket, may stay
  quiet as long as they like.
- **Ports that speak TLS** get a TLS connection without certificate
  checks; anything else is plain HTTP. A port speaks TLS when its
  `appProtocol` is `https` or `wss` (or `kubernetes.io/wss`), else when
  it's named `https` or `wss`, else when it's 443 and neither its
  `appProtocol` nor its name says HTTP. Values starting with `https-`
  or `wss-` count too, in either.
- **The `Host` header** is passed on as received. An app whose own URL
  setting names its public domain may redirect there.
- **Requests other sites make** are refused with a 403, so a web page
  can't reach a cluster with your credentials: a request whose `Origin`
  or `Referer` is a page outside carte's TLDs and loopback, whose
  `Origin` is hidden (`null`), or that the browser marks cross-site.
  Opening a page yourself always works, including by a link from
  another site, but another site can't show a Service in a frame or
  have the browser prefetch or prerender it before you click. An
  SSO login that returns to the Service by POST (SAML, or OIDC with
  `response_mode=form_post`) is refused, since it's a cross-site POST.

Starts, pod changes, failures and idle stops show in `carte events`.
When the pod side of a forward fails, as when nothing listens on the
port, the error page, `carte status`, `carte ls`, the terminal UI and
`carte events` show what the cluster said, until the pod answers again.

`carte logs grafana.prod` shows the logs of the pod the forward uses,
which is the pod every one of your requests reaches, or of a ready pod
when no forward runs; reading logs doesn't open one. Of a pod's
containers it picks the one serving the Service's port, rather than a
sidecar. `--search` and `--since` look through up to 10,000 lines.
Following (`-f`) continues on the next pod when this one goes away. Logs
stream only while a client reads them, and reading them needs the
`pods/log` permission, which is separate from port-forwarding.

In the terminal UI, a kube project starts closed. Opening it asks the
cluster for all its Services, once per opening, and lists them by
namespace beside the ones carte already knows of: namespaces where
something runs or failed are open, the others closed with a count. `/` finds
Services in closed namespaces, `o` and `y` open and copy a Service's
URL, and `s` starts its forward. When the cluster doesn't let you list
Services across namespaces, carte lists them per namespace, and when it
doesn't let you list namespaces either, it asks you to name the ones you
use in `namespaces`.

The project's row shows its context, or why the cluster can't be
reached, like a failed login; a Service's details show the pod it
forwards to, and its log pane shows its pod's logs.
