# Configuration

carte reads three kinds of TOML files:

| File | Holds |
| --- | --- |
| `~/.config/carte/config.toml` | Machine settings: TLD, ports, idle timeout, limits, sharing |
| `~/.config/carte/projects/<name>.toml` | One project: its repos, env and per-app settings |
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
tld = "test"
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
| `tld` | `"test"` | The top-level domain apps are served under |
| `http_port` | `80` | Plain HTTP |
| `https_port` | `443` | HTTPS |
| `dns_port` | `9253` | carte's DNS server, which `/etc/resolver/<tld>` points to |
| `idle_timeout` | `"15m"` | How long an app runs without traffic before it's stopped; projects can override it |

Changing `tld` or the ports means running `carte setup` again.

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
repos = "~/work/acme-*"
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

Apps can also be listed one by one, or mixed with globs:

```toml
repos = ["~/work/acme-billing", "~/work/acme-frontend", "~/clients/acme/*"]
```

| Key | Default | |
| --- | --- | --- |
| `repos` | required | A path or glob, or a list mixing both. Each match that is a git repository (not a worktree) is an app. Repos that start matching a glob later join the project; an entry that matches nothing is reported |
| `strip_prefix` | none | Removed from directory names to get the short names used in hostnames |
| `env_provider` | detected | Where the base environment comes from: `mise`, `direnv`, `shell` or `none`. See [Environment](#environment) |
| `env` | none | Env templates for every app. See [Env templates](#env-templates) |
| `requires` | none | Services that must be reachable before anything in the project starts: `tcp://host:port` or `unix:///path` |
| `scheme` | `"https"` | The scheme of URLs carte injects. `"http"` for apps that misbehave behind HTTPS |
| `idle_timeout` | machine's | Overrides the machine's idle timeout for this project |
| `apps` | none | Per-app settings, keyed by directory name. See [Apps](#apps) |

### Names and hostnames

An app's name is its directory name, and its short name is that name
with `strip_prefix` removed (or `name`, if set). Hostnames are built from
short names, project and stack:

| | Hostname |
| --- | --- |
| On main | `billing.acme.test` |
| On a branch | `billing.feature-x.acme.test` |
| An app whose short name is the project's | `acme.test`, `feature-x.acme.test` |
| Shared | `billing--feature-x--acme.<share domain>` |

Short names must be at least three characters and unique within the
project. A branch can't be named like an app of its project: the app
wins, and the stack is reported as unreachable.

## Apps

`[apps.<directory name>]` tables in a project file, or the top level of a
repo's `carte.toml`. When both set a key, the project file wins, key by
key; `env` and `processes` are merged entry by entry.

```toml
[apps.acme-events]
name = "events"
subdomains = true
aliases = ["acme-events"]
env_files = [".env"]
depends_on = ["accounts", "mail.other-project"]
policy = "pinned"

[apps.acme-events.env]
TZ = "Europe/Oslo"
```

| Key | Default | |
| --- | --- | --- |
| `name` | directory name minus `strip_prefix` | The short name used in hostnames |
| `env` | none | Env templates for this app, over the project's |
| `env_files` | none | Files of `KEY=VALUE` lines, relative to the checkout, read in order |
| `depends_on` | none | Apps this app calls. See below |
| `aliases` | none | Extra hostnames, without the TLD, routed to the app on main. `carte import puma-dev` sets the old puma-dev names |
| `subdomains` | `false` | Pass extra labels on the left (`oslo.events.acme.test`) through to the app instead of returning 404 |
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
| `kind` | none | `"service"` or `"task"`, which start before the other processes. See [Services and tasks](#services-and-tasks) |
| `after` | none | Processes of the same kind this one starts after, like `["migrate"]` |

Every process gets its own `PORT`. A fixed port in the HTTP process's
command (`-p 3000`, `--port=3000`) is replaced with `$PORT`, with a
notice.

Processes that look like file watchers default to `scope = "checkout"`:
names containing `watch`, `css` and `js`, and commands with `-w`,
`--watch` or a `:watch` task.

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
- Requests wait for the whole sequence.
- Tasks run on every start of the instance, including after an idle
  stop, so they must be quick when there's nothing to do. A finished task
  shows as `done` and isn't run again until the next start; restarting
  one process doesn't run them.
- Stopping goes the other way: services get their signal once
  everything else has exited, so a job worker can hand its unfinished
  jobs back to Redis.
- A disabled service or task is skipped, and nothing waits for it.
- After the start, a service that exits on its own is restarted on the
  same port with the same env, so sockets and URLs other processes use
  stay valid. carte waits 1s before the first restart and doubles the
  wait each time, up to 32s; at the fifth crash within a minute it gives
  up, and the service stays down while the instance keeps running.
  Restarts and the give-up show in `carte events`. Stopping it by hand
  doesn't trigger a restart. Other processes aren't restarted.
- `after` orders processes within a tier: a task with
  `after = ["migrate"]` starts once `migrate` has finished, a process
  after `web` once `web` is ready.

The HTTP process can't have a `kind`, and services and tasks always run
per instance, never with `scope = "checkout"`.

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
  `server.allowedHosts`.
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
4. **Variables carte sets:**

| Variable | |
| --- | --- |
| `PORT` | The process's port |
| `CARTE_APP`, `CARTE_PROJECT` | App and project names |
| `CARTE_STACK` | `main`, or the branch slug of the stack |
| `CARTE_CHECKOUT` | `main`, or the branch slug of the worktree the code runs from |
| `CARTE_BRANCH`, `CARTE_WORKTREE` | The checkout's branch and path |
| `CARTE_STATE_DIR` | The instance's [state directory](#instance-directories) |
| `CARTE_RUN_DIR` | The instance's [run directory](#instance-directories) |
| `CARTE_URL` | The app's URL in this stack |
| `CARTE_PUBLIC_URL` | Its public URL, while shared |
| `CARTE_TLD_LENGTH` | Labels after the app's own, for Rails' `tld_length` |
| `PIDFILE` | A pidfile path for the instance, in carte's state directory |
| `RAILS_DEVELOPMENT_HOSTS` | The project's domain and the share domain |
| `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS` | The same, for Vite's `server.allowedHosts` |
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
  `CARTE_RUNTIME_HOME` in the daemon's environment moves it.

Commands aren't templated, so they use the variables like `$PORT`. A
process that wants a subdirectory creates it:

```
redis: mkdir -p $CARTE_STATE_DIR/redis && exec redis-server --port $PORT --unixsocket $CARTE_RUN_DIR/redis.sock --dir $CARTE_STATE_DIR/redis
```

```toml
[env]
REDIS_URL = "unix://{{run_dir}}/redis.sock"
```

The stack's folder, `<state>/data/<project>/<stack>`, also holds
`stack.toml`, where carte records the stack's branch and each app's
worktree.

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
| `stack.id` | 8 lowercase letters and digits from a hash of project and stack; the same for a recreated branch and on every machine |
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
