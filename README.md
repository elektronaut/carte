# carte

carte runs your local web apps on demand. Visit `https://myapp.localhost` and
carte starts the app's processes, waits until it answers, and forwards the
request. After 15 minutes without traffic, it stops them again.

It's built for many apps at once, and for git worktrees: each worktree
gets its own hostname, named after its folder, and apps without a
worktree of that name run from their main checkout, so you only create
worktrees for the services you change.

- **The whole process group**, not only the web server: everything in
  `Procfile.dev` starts and stops together, including asset watchers,
  job workers and schedulers.
- **Stacks per worktree:** `billing.feature-x.acme.localhost` is the billing
  app in its `feature-x` worktree, and its calls to other apps stay in
  the `feature-x` stack.
- **HTTPS everywhere,** with a local certificate authority your apps and
  browsers trust.
- **A terminal UI** for state, logs and controls.
- **Sharing** apps on the public internet through Cloudflare Tunnel or
  ngrok.

carte is macOS-only for now. The daemon runs on Linux, but `carte setup`
doesn't support it yet.

## Alternatives

carte is heavily inspired by [puma-dev](https://github.com/puma/puma-dev). 
The `.test` hostnames, starting apps on request, idle shutdown and 
`tmp/restart.txt` all come from it. If you want a simpler solution that 
just gives you a web server per app, it's highly recommended.

[Overmind](https://github.com/DarthSim/overmind) and
[process-compose](https://github.com/F1bonacc1/process-compose) run a
project's processes in your terminal, with no daemon, hostnames or idle
shutdown. They're a good fit if you work on one app at a time and want a
better `bin/dev`.

[portless](https://github.com/vercel-labs/portless) gives dev servers you
start yourself stable `.localhost` names, including one per worktree. It
routes, but leaves starting and stopping processes to you.

## Install

With Homebrew:

```sh
brew install elektronaut/tap/carte
```

Or with Cargo:

```sh
cargo install carte
```

## Setup

```sh
carte setup --dry-run   # show what it will do
carte setup
```

Setup does this once per machine, asking for your password where needed:

- creates a local certificate authority and trusts it
- installs a LaunchAgent that runs the daemon on ports 80 and 443, on
  127.0.0.1 and ::1
- stops puma-dev, if it's running, and leaves `~/.puma-dev` alone

`carte setup --check` reports what's in place, and
`carte setup --uninstall` undoes it all.

Apps are served under `.localhost`, which resolves to your machine without
any DNS setup. For another TLD, like `.test`, run `carte setup --tld test`:
setup then also sends `.test` lookups to carte's DNS server through
`/etc/resolver/test`. Apps answer under `.localhost` either way.

### Coming from puma-dev

```sh
carte import puma-dev
```

This creates a project for each app linked in `~/.puma-dev`. Each app
keeps its puma-dev name as an alias. `carte setup` then asks whether to
keep puma-dev's `.test` hostnames, so bookmarks, `.env` URLs and OAuth
callbacks keep working, or switch to `.localhost` and remove puma-dev's
resolver files (`--tld test` or `--tld localhost` answers up front). `.env` and `.powenv` files are read as
before. puma-dev's own files stay untouched, and
`carte setup --uninstall` tells you how to bring puma-dev back.

## Projects

A project is a group of apps that belong together. Each project is a
file in `~/.config/carte/projects/`. For a single app, this is enough:

```sh
cd ~/code/myapp
carte project add .
```

That writes `~/.config/carte/projects/myapp.toml` with one line:

```toml
root = "~/code/myapp"
```

and the app is at `https://myapp.localhost`. Its processes come from
`Procfile.dev`, then `Procfile`. Without either, a `config.ru` runs
`bundle exec puma -p $PORT`. Every process gets its own `PORT`.

A larger project:

```toml
# ~/.config/carte/projects/acme.toml
root = "~/work"
include = "acme-*"
strip_prefix = "acme-"
requires = ["tcp://localhost:5432"]

[env]
"{{app.name | upper}}_URL" = "{{url:app}}"
DATABASE_URL = "postgres:///{{app.name}}_{{checkout}}"

[apps.acme-frontend]
depends_on = ["billing", "accounts"]

[apps.acme-billing.processes.worker]
policy = "disabled"
```

- `root` is where the project lives, and `include` lists the
  directories under it that are apps: paths and globs, with `!` in
  front to leave matches out. A glob only picks directories with
  something to run, and new ones join as they appear. Without
  `include`, the root is the one app, named after the project.
- A monorepo works the same way: `root = "~/work/mono"` and
  `include = "apps/*"`. Each worktree of it is a stack, with the apps
  that worktree has.
- For a directory full of unrelated apps, `projects = "*"` makes the
  file a template with a project per directory. See
  [Templates](docs/configuration.md#templates).
- `strip_prefix` shortens hostnames: `acme-billing` is
  `billing.acme.localhost`. An app named like its project is just
  `acme.localhost`.
- `requires` is checked before anything in the project starts, so a
  stopped database gives you one clear error instead of a crash per app.
- `[env]` templates apply to every app. A template key, like the one
  above, gives every app its own variable, here `ACME_BILLING_URL` and so
  on.
- `depends_on` starts the apps an app calls along with it, and keeps them
  running while it runs. An app in another project is named by its
  project, or `app.project`.
- `policy` is `auto` (the default), `pinned` (never idles out) or
  `disabled` (never starts unless you start it).
- A process with `kind = "service"`, like a Redis server, starts and is
  ready before the others, and stops after them. One with
  `kind = "task"`, like `bin/rails db:prepare`, runs to completion
  between the services and the rest, on every start. One with
  `kind = "teardown"`, like `dropdb`, runs once in the main checkout
  when a stack's worktrees have been gone for `teardown_delay` (10
  minutes), and then carte deletes the stack's data.
- A process other than the web process and tasks that crashes after the
  start is restarted on the same port, with a growing wait; carte gives
  up at the fifth crash within a minute. `restart = "never"` leaves it
  down.
- `[processes.<name>]` declares a process for several apps at once, all
  of them or those in `apps`; an app's own `[apps.<app>.processes]`
  entry overrides it key by key. Its `provides` sets env on every process
  of the app, so each app's Redis hands out its own socket:

```toml
[processes.redis]
apps = ["billing", "events"]
command = "exec redis-server --port $PORT --unixsocket $CARTE_RUN_DIR/redis.sock --save ''"
kind = "service"

[processes.redis.provides]
REDIS_URL = "unix://{{run_dir}}/redis.sock"
```

A repo can carry its own defaults in a `carte.toml`, using the same keys
as an `[apps.<name>]` table. The project file wins.

Project files can live in subdirectories of `projects/`, which show as
folders in the terminal UI.

[docs/configuration.md](docs/configuration.md) describes every setting.

### Environment

Each process's environment is built in layers, later ones winning:

1. **mise**, when the checkout has `mise.toml` or `.tool-versions`;
   otherwise your login shell's. Set `env_provider` to `mise`, `direnv`,
   `shell` or `none` to choose.
2. **Env files** listed in `env_files`.
3. **Templates** from `[env]`.
4. **Provided values** from the `provides` of the app's processes.
5. **carte's own:** `PORT`, `CARTE_URL`, `CARTE_STACK`,
   `CARTE_CHECKOUT` and others, plus `SSL_CERT_FILE` and
   `NODE_EXTRA_CA_CERTS` so Ruby, Node, Python and curl trust the local
   certificate authority.

`carte env` shows the result, with where each value comes from.

## Worktrees and stacks

Stacks are never created by hand. A worktree's stack is named after
its folder, and worktrees of different repos in a project with the same
name form one stack, whatever branch each has checked out:

```sh
cd ~/work/acme-billing
git worktree add ~/worktrees/acme-billing/feature-x -b feature-x
```

Now `https://billing.feature-x.acme.localhost` runs the worktree, and
`https://frontend.feature-x.acme.localhost` runs the frontend's main checkout
as a separate instance whose URLs point into the `feature-x` stack. When
you remove the worktree, carte stops its instance and the stack falls
back to main.

When a worktree's folder has the repo's own name, the stack takes the
name of the folder above it. That suits a folder per task with a
checkout of each repo inside:

```
~/work/workspaces/checkout-flow/acme-billing    stack checkout-flow
~/work/workspaces/checkout-flow/acme-frontend   stack checkout-flow
```

Worktrees next to the main checkout keep their whole folder name, so
`../acme-billing-feature-x` is the stack `acme-billing-feature-x`, and
`.worktrees/feature-x` inside the repo is `feature-x`. Names are
lowercased, with anything outside `a-z`, `0-9` and `-` turned into a
dash.

Two worktrees of one repo can't share a stack: when their folders have
the same name, carte reports it as a problem. Switching branches or a
rebase inside a worktree never changes a stack's name, but moving or
renaming its folder (`git worktree move`) does. The checkout then starts
fresh under the new name, and the old stack is torn down after
`teardown_delay`, running its [teardown processes](docs/configuration.md#teardown),
such as dropping its database.

carte doesn't touch databases. Use `CARTE_CHECKOUT` in `database.yml` or
similar if you want separate data per worktree.

Each app in a stack gets a state directory, `CARTE_STATE_DIR`, for data
that should persist, and a run directory, `CARTE_RUN_DIR`, with a path
short enough for Unix sockets.

## Everyday use

Most of the time you don't need the CLI: visiting an app starts it.

```sh
carte                        # the terminal UI
carte ls                     # what's running
carte up billing@feature-x   # start an app in a stack
carte up @feature-x          # start a whole stack
carte restart billing/worker # restart one process
carte logs -f billing        # follow logs
carte pin billing --save     # keep it running, and save that
carte open                   # open the app you're in
carte exec -- bin/rails c    # a command with the app's env
carte stop-all --idle        # stop what nobody is using
carte prune                  # remove data of stacks and apps that are gone
```

Addresses are `app@stack/process`, each part optional and inferred from
the current directory. Any unambiguous part of a name works.
`--project` addresses a project, even when one of its apps has the same
name. Every command takes `--json`.

`touch tmp/restart.txt` restarts an app, as with puma-dev.

[docs/cli.md](docs/cli.md) has every command, the JSON output and exit
codes.

Coding agents can use carte through `carte mcp`, an MCP server; see
[docs/mcp.md](docs/mcp.md).

### The terminal UI

`carte` without a command opens it: projects, stacks, apps and processes
on the left, logs on the right. `?` lists every key.

| Key | Does |
| --- | --- |
| `s` `x` `r` | start, stop, restart the selection |
| `p` `d` | pin or disable; `S` saves it to the project file |
| `o` | open in the browser |
| `w` `W` | share, or share behind a password |
| `a` | show everything, what's running or what's shared |
| `/` | filter |
| drag | copy log lines |

## Sharing

Sharing puts chosen apps on the public internet, for a demo or a webhook,
and is always explicit.

### Cloudflare Tunnel

Needs a domain on Cloudflare, with a subdomain or whole domain per
machine:

```sh
brew install cloudflared
cloudflared tunnel login
carte setup --share dev.example.com
```

Apps get hostnames like `billing--feature-x--acme.dev.example.com`, and
any number of apps can be shared at once.

### ngrok

Works with ngrok's free plan and its free static domain, one app at a
time:

```sh
brew install ngrok
ngrok config add-authtoken <token>
carte setup --share <your-domain>.ngrok-free.dev
```

### Sharing an app

```sh
carte share billing@feature-x --apps frontend --ttl 2h --auth
carte unshare @feature-x
```

`--auth` adds a password, `--ttl` ends the share by itself, and `--pin`
keeps the apps running so visitors never wait for a boot. Shares end when
the daemon restarts. Only shared apps are reachable; everything else
returns 404.

## Configuration

Machine settings live in `~/.config/carte/config.toml`, and every key is
optional:

```toml
tld = "test"
idle_timeout = "15m"

[limits]
memory_target = "50%"   # above this, stop the least recently used apps
hard_ceiling = 50       # most instances at once

[share]
provider = "cloudflare"
domain = "dev.example.com"
```

carte reloads configuration when files change. A broken file keeps the
last working version, and `carte ls` and the terminal UI show the error.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. See [LICENSE](LICENSE).
