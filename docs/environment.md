# Environment

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
   See [Services and tasks](processes.md#services-and-tasks).
5. **Variables carte sets:**

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
| `CARTE_TLD_LENGTH` | Labels after the app's own, for Rails' `tld_length`. It counts from the app's own hostname, so it doesn't fit an alias with fewer labels. With `subdomains = true`, `oslo.frontend.nova.localhost` gives Rails' `request.subdomain` `oslo`, but the alias `oslo.nova-frontend.localhost` gives an empty one |
| `PIDFILE` | A pidfile path for the instance, in carte's state directory |
| `RAILS_DEVELOPMENT_HOSTS` | Every domain the instance answers under, for Rails' `config.hosts`: the project's, the stack's, the app's own with `subdomains = true`, its aliases on main, and the share domain |
| `__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS` | The project's domain, aliases on main and the share domain, for Vite's `server.allowedHosts` |
| `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` | System roots plus carte's CA |
| `NODE_EXTRA_CA_CERTS` | carte's CA |
| `DISABLE_SPRING` | `1` |
| `CARTE_SHIM` | Names the process carte runs the command under; set by carte itself, not by the config. If carte crashes along with it, the next start stops the process groups whose processes still carry it |

When a mise config file (`mise.toml`, `mise.local.toml`, `.tool-versions`
and the like) or a version file (`.node-version`, `.nvmrc`,
`.ruby-version`, `.python-version`) changes in an app's checkout or its
repo's root, running apps are marked "env changed" rather than
restarted. Changes in other directories, such as `.config/mise/` or a
folder between the repo's root and the app, aren't watched: restart the
app after them. `carte env <app>` prints the result
with where each value came from.

## Instance directories

Each instance (an app in a stack) gets two directories of its own. carte
creates them before its processes start, and creates nothing inside them.

- The **state directory**, `CARTE_STATE_DIR`, is for data that should
  outlive a restart, such as a database's files. It's
  `<state>/data/<project>/<stack>/<app>` in carte's state directory
  (`~/.local/state/carte` by default).
- The **run directory**, `CARTE_RUN_DIR`, is for sockets and other
  files that don't need to survive a reboot. Its path is short, so a Unix
  socket in it stays under the operating system's limit of about 100
  bytes: a hash of carte's state directory, project, stack and app in
  the per-user temp directory on macOS, in `$XDG_RUNTIME_DIR/carte` or `/tmp/carte-<uid>` on Linux.
  `CARTE_RUNTIME_HOME` in the daemon's environment moves it; `carte env`
  shows the daemon's path while it runs. The folder and each run
  directory must be real directories owned by you: carte refuses a
  symlink or another user's folder, and closes the folder to other
  users. Beside each run directory, a `<hash>.owner` file names the
  carte that made it, by its state directory's `data` folder. carte
  writes it only when it's missing, and refuses a run directory whose
  file names another carte.

Commands aren't templated, so they use the variables like `$PORT`. A
process that wants a subdirectory creates it:

```
redis: mkdir -p $CARTE_STATE_DIR/redis && exec redis-server --port $PORT --unixsocket $CARTE_RUN_DIR/redis.sock --dir $CARTE_STATE_DIR/redis
```

Templates use `{{state_dir}}` and `{{run_dir}}`. The process's
[`provides`](processes.md#services-and-tasks) gives the socket to the rest of the app:

```toml
[apps.acme-billing.processes.redis.provides]
REDIS_URL = "unix://{{run_dir}}/redis.sock"
```

The stack's folder, `<state>/data/<project>/<stack>`, also holds
`stack.toml`, where carte records each app's worktree, and the last
branch seen in them.

When a stack's worktrees are gone, its [teardown](processes.md#teardown) deletes
its directories. carte doesn't delete them when an app or project goes
away or when a teardown fails; [`carte prune`](cli.md#prune) removes
those leftovers. It removes a run directory only when its `.owner` file
names this carte, and deletes the `.carte-prune-*` folders an
interrupted prune left.

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

## Env files

Each line of an env file is `KEY=VALUE`, optionally after `export `.
Blank lines and lines starting with `#` are skipped.

- An unquoted value runs to the end of the line, or to ` #`, which starts
  a comment. Surrounding spaces are dropped.
- A single-quoted value is taken as is.
- A double-quoted value can span lines, and understands `\n`, `\t`,
  `\r`, `\"`, `\\` and `\$`.
- `${VAR}` and `$VAR` are kept as written, not expanded.
