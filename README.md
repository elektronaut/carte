# carte

carte runs your local web apps on demand. Visit `https://myapp.test` and
carte starts the app's processes, waits until it answers, and forwards the
request. After 15 minutes without traffic, it stops them again.

It's built for many apps at once, and for git worktrees: each branch you
check out in a worktree gets its own hostname, and apps without a
worktree for that branch run from their main checkout, so you only create
worktrees for the services you change.

- **The whole process group**, not only the web server: everything in
  `Procfile.dev` starts and stops together, including asset watchers,
  job workers and schedulers.
- **Stacks per branch:** `billing.feature-x.acme.test` is the billing app
  on the `feature-x` branch, and its calls to other apps stay on that
  branch.
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

- sends `.test` lookups to carte's DNS server (`/etc/resolver/test`)
- creates a local certificate authority and trusts it
- installs a LaunchAgent that runs the daemon on ports 80 and 443
- stops puma-dev, if it's running, and leaves `~/.puma-dev` alone

`carte setup --check` reports what's in place, and
`carte setup --uninstall` undoes it all.

### Coming from puma-dev

```sh
carte import puma-dev
```

This creates a project for each app linked in `~/.puma-dev`. Each app
keeps its puma-dev hostname as an alias, so bookmarks, `.env` URLs and
OAuth callbacks keep working. `.env` and `.powenv` files are read as
before. puma-dev's own files stay untouched, and
`carte setup --uninstall` tells you how to bring puma-dev back.

## Projects

A project is a group of repos that belong together. Each project is a
file in `~/.config/carte/projects/`. For a single app, this is enough:

```sh
cd ~/code/myapp
carte project add .
```

That writes `~/.config/carte/projects/myapp.toml` with one line:

```toml
repos = "~/code/myapp"
```

and the app is at `https://myapp.test`. Its processes come from
`Procfile.dev`, then `Procfile`. Without either, a `config.ru` runs
`bundle exec puma -p $PORT`. Every process gets its own `PORT`.

A larger project:

```toml
# ~/.config/carte/projects/acme.toml
repos = "~/work/acme-*"
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

- `repos` is a path, a glob, or a list of them. New repos that match a
  glob join the project.
- `strip_prefix` shortens hostnames: `acme-billing` is
  `billing.acme.test`. An app named like its project is just
  `acme.test`.
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
4. **carte's own:** `PORT`, `CARTE_URL`, `CARTE_STACK`,
   `CARTE_CHECKOUT` and others, plus `SSL_CERT_FILE` and
   `NODE_EXTRA_CA_CERTS` so Ruby, Node, Python and curl trust the local
   certificate authority.

`carte env` shows the result, with where each value comes from.

## Worktrees and stacks

Stacks are never created by hand. When repos in a project have
worktrees on the same branch, they form a stack:

```sh
cd ~/work/acme-billing
git worktree add ../acme-billing-feature-x -b feature-x
```

Now `https://billing.feature-x.acme.test` runs the worktree, and
`https://frontend.feature-x.acme.test` runs the frontend's main checkout
as a separate instance whose URLs point into the `feature-x` stack. When
you remove the worktree, carte stops its instance and the stack falls
back to main.

carte doesn't touch databases. Use `CARTE_CHECKOUT` in `database.yml` or
similar if you want separate data per worktree.

## Everyday use

Most of the time you don't need the CLI: visiting an app starts it.

```sh
carte                        # the terminal UI
carte ls                     # what's running
carte up billing@feature-x   # start an app on a branch
carte up @feature-x          # start a whole stack
carte restart billing/worker # restart one process
carte logs -f billing        # follow logs
carte pin billing --save     # keep it running, and save that
carte open                   # open the app you're in
carte stop-all --idle        # stop what nobody is using
```

Addresses are `app@branch/process`, each part optional and inferred from
the current directory. Any unambiguous part of a name works.
`--project` addresses a project, even when one of its apps has the same
name. Every command takes `--json`.

`touch tmp/restart.txt` restarts an app, as with puma-dev.

[docs/cli.md](docs/cli.md) has every command, the JSON output and exit
codes.

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
