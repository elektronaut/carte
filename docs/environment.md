# Environment

Each process's environment is built in layers, later ones winning:

1. **The provider,** which gives the base environment. See below.
2. **Env files** from `env_files`.
3. **Templates:** the project's `[env]`, then the app's, then the
   process's.
4. **Provided values** from the `provides` of the instance's processes.
   See [Services and tasks](processes.md#services-and-tasks).
5. **Variables carte sets.** See the table below.

`carte env <app>` prints the result, with where each value came from.

## Providers

`env_provider` picks the provider. Without it, carte detects one:

- **`mise`** when mise is installed and has config for the app. carte
  looks for config in the app's directory and in each one above it, up
  to the repo's root. Config is `mise.toml`, `.tool-versions` or another
  of mise's config files, such as `mise.local.toml` or
  `.config/mise/config.toml`.
- **`shell`** otherwise: the environment of a login shell in the
  checkout.

Outside a repo, carte only looks in the app's own directory. Above the
app's directory, it never looks in your home directory, which holds
mise's global config.

With `shell`, when the shell's `PATH` has mise's shims, carte adds
`mise env` on top. A shim would otherwise apply mise's `[env]` over
carte's values inside every process.

Two providers are never detected, and only run when set:

- **`direnv`** uses `direnv export json`.
- **`none`** uses only the daemon's own environment.

The provider has 30 seconds to finish. After that, carte stops it and
every process it started, and the app fails to start. So let a slow
first run, such as `use flake` in `.envrc`, finish in a shell in the
checkout first.

When a config or version file changes in an app's checkout or its
repo's root, running apps are marked "env changed" rather than
restarted. That covers mise's config files (`mise.toml`,
`mise.local.toml`, `.tool-versions` and the like) and version files
(`.node-version`, `.nvmrc`, `.ruby-version`, `.python-version`).

Changes in other directories aren't watched. That includes
`.config/mise/`, and folders between the repo's root and the app.
Restart the app after changing them.

## Variables carte sets

| Variable | |
| --- | --- |
| `PORT` | The process's port |
| `CARTE_APP`, `CARTE_PROJECT` | App and project names |
| `CARTE_STACK` | `main`, or the stack's [name](projects.md#names-and-hostnames) |
| `CARTE_CHECKOUT` | `main`, or the stack name of the worktree the code runs from |
| `CARTE_BRANCH`, `CARTE_WORKTREE` | The checkout's branch (empty on a detached HEAD) and path |
| `CARTE_STATE_DIR` | The instance's [state directory](#instance-directories) |
| `CARTE_RUN_DIR` | The instance's [run directory](#instance-directories) |
| `CARTE_URL` | The app's URL in this stack |
| `CARTE_PUBLIC_URL` | Its public URL, while shared |
| `CARTE_TLD_LENGTH` | Labels after the app's own, for Rails' `tld_length`. See below |
| `PIDFILE` | A pidfile path for the instance, in carte's state directory |
| `RAILS_DEVELOPMENT_HOSTS` | Every domain the instance answers under, for Rails' `config.hosts`. See below |
| `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS` | The project's domain, aliases on main and the share domain, for Vite's `server.allowedHosts` |
| `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` | System roots plus carte's CA |
| `NODE_EXTRA_CA_CERTS` | carte's CA |
| `DISABLE_SPRING` | `1` |
| `CARTE_SHIM` | Names the process carte runs the command under. See below |

**`CARTE_TLD_LENGTH`** counts from the app's own hostname, so it doesn't
fit an alias with fewer labels. Take an app with `subdomains = true`.
On `oslo.frontend.nova.localhost`, Rails' `request.subdomain` is
`oslo`. On the alias `oslo.nova-frontend.localhost`, it's empty.

**`RAILS_DEVELOPMENT_HOSTS`** lists:

- the project's domain
- the stack's domain
- the app's own domain, with `subdomains = true`
- its aliases, on main
- the share domain

**`CARTE_SHIM`** is set by carte itself, not by the config. If carte
crashes along with the process, the next start stops the process groups
whose processes still carry it.

## Instance directories

Each instance, an app in a stack, gets two directories of its own. carte
creates them before its processes start, and creates nothing inside
them.

The **state directory**, `CARTE_STATE_DIR`, is for data that should
outlive a restart, such as a database's files. It's
`<state>/data/<project>/<stack>/<app>` in carte's state directory,
which is `~/.local/state/carte` by default.

The **run directory**, `CARTE_RUN_DIR`, is for sockets and other files
that don't need to survive a reboot. Its path is short, so a Unix socket
in it stays under the operating system's limit of about 100 bytes. It's
named by a hash of carte's state directory, project, stack and app, in:

- the per-user temp directory on macOS
- `$XDG_RUNTIME_DIR/carte` or `/tmp/carte-<uid>` on Linux

`CARTE_RUNTIME_HOME` in the daemon's environment moves it. While the
daemon runs, `carte env` shows the daemon's path.

carte checks the run directories before it uses them:

- The folder and each run directory must be real directories owned by
  you. carte refuses a symlink or another user's folder.
- carte closes the folder to other users.
- Beside each run directory, a `<hash>.owner` file names the carte that
  made it, by its state directory's `data` folder. carte writes it only
  when it's missing, and refuses a run directory whose file names
  another carte.

Commands aren't templated, so they use the variables, like `$PORT`. A
process that wants a subdirectory creates it:

```
redis: mkdir -p $CARTE_STATE_DIR/redis && exec redis-server --port $PORT --unixsocket $CARTE_RUN_DIR/redis.sock --dir $CARTE_STATE_DIR/redis
```

Templates use `{{state_dir}}` and `{{run_dir}}`. The process's
[`provides`](processes.md#services-and-tasks) gives the socket to the
rest of the app:

```toml
[apps.acme-billing.processes.redis.provides]
REDIS_URL = "unix://{{run_dir}}/redis.sock"
```

The stack's folder, `<state>/data/<project>/<stack>`, also holds
`stack.toml`. There carte records each app's worktree, and the last
branch seen in them.

When a stack's worktrees are gone, its
[teardown](processes.md#teardown) deletes its directories. carte doesn't
delete them when an app or project goes away, or when a teardown fails.
[`carte prune`](cli.md#prune) removes those leftovers:

- It removes a run directory only when its `.owner` file names this
  carte.
- It deletes the `.carte-prune-*` folders an interrupted prune left.

## Env templates

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
| `stack.id` | 8 lowercase letters and digits from a hash of project and stack name. The same for a recreated worktree, and on every machine |
| `port` | The process's port |
| `state_dir`, `run_dir` | The instance's [directories](#instance-directories), as in `CARTE_STATE_DIR` and `CARTE_RUN_DIR` |
| `url:<app>` | Another app's URL in the same stack. `url:app` is the app itself |
| `public_url:<app>` | Another app's public URL. An error while it isn't shared |
| `host:<app>` | The hostname of `url:<app>`, without scheme or port: `billing.feature-x.acme.localhost` |
| `public_host:<app>` | The hostname of `public_url:<app>`. An error while it isn't shared |

Filters, applied in order:

| Filter | `acme-billing` becomes |
| --- | --- |
| `upper` | `ACME_BILLING` |
| `snake` | `acme_billing` |
| `slug` | `acme-billing`, lowercased with other characters turned into dashes |

A key with a template in it is a **key template**. It's rendered once per
app in the project, with `app` meaning each app in turn. This gives
every app a variable pointing at every other:

```toml
[env]
"{{app.short | upper}}_URL" = "{{url:app}}"
# BILLING_URL, FRONTEND_URL, ACCOUNTS_URL, …
```

A key template that uses `public_url:app` or `public_host:app`, in its
key or its value, skips apps that aren't shared.

Variable names are letters, digits, underscores and dots, not starting
with a digit or dot. Dots allow Spring's `server.port` style. A name
that breaks these rules is an error, whether written or rendered.

For a literal `{{`, write `{{"{{"}}` in a single-quoted TOML string.
`DOCKER_FORMAT = '{{"{{"}}.Names}}'` gives `{{.Names}}`.

## Env files

Each line of an env file is `KEY=VALUE`, optionally after `export `.
Blank lines and lines starting with `#` are skipped.

- An unquoted value runs to the end of the line, or to ` #`, which starts
  a comment. Surrounding spaces are dropped.
- A single-quoted value is taken as is.
- A double-quoted value can span lines, and understands `\n`, `\t`,
  `\r`, `\"`, `\\` and `\$`.
- `${VAR}` and `$VAR` are kept as written, not expanded.
