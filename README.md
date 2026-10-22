# carte

Run your local web apps _à la carte_: visit `https://myapp.localhost` and
carte starts the app, then stops it once you're done with it.

- **Many apps at once:** carte stops the least recently used ones to
  keep memory in check.
- **The whole Procfile:** asset watchers, job workers and schedulers
  start and stop with the web server.
- **A hostname per worktree:** a worktree in a `feature-x` folder runs
  at `https://feature-x.myapp.localhost`.
- **Projects and stacks:** apps that work together start together, and
  worktrees with the same name form a stack.
- **HTTPS everywhere,** with a local certificate authority.
- **A terminal UI** for state, logs and controls.
- **Sharing** apps on the public internet through Cloudflare Tunnel or
  ngrok.
- **Kubernetes Services** at hostnames like
  `https://grafana.monitoring.prod.localhost`, forwarded when you visit.

carte runs on macOS and Linux.

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

Setup runs once per machine and asks for your password where needed. It:

- creates a certificate authority for carte's hostnames and trusts it
- runs the daemon on ports 80 and 443. On Linux, this lets every program
  use ports from 80 up.

Apps are served under `.localhost`. To also serve them under `.test`, run
`carte setup --tld test`.

`carte setup --check` shows what's in place, and `carte setup --uninstall`
removes it.

### Coming from puma-dev

`carte setup` offers to replace puma-dev, and to keep its `.test`
hostnames. To turn the apps linked in `~/.puma-dev` into carte projects:

```sh
carte import puma-dev
```

[docs/puma-dev.md](docs/puma-dev.md) compares the two and covers the
switch in detail.

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

and the app is at `https://myapp.localhost`.

carte runs the processes in the app's `Procfile.dev` or `Procfile`. Without
one, it runs a Rack app with Puma and a Node app with its `dev` script.

A project can also hold several apps that work together. Say the acme
product is three repos in `~/work`, next to some unrelated ones:

```
~/work/
├── acme-accounts/    Rails API
├── acme-billing/     Rails API
├── acme-frontend/    Vite app calling the other two
└── side-project/
```

One file makes them a project:

```toml
# ~/.config/carte/projects/acme.toml
root = "~/work"
include = "acme-*"
strip_prefix = "acme-"

[env]
"{{app.name | upper}}_URL" = "{{url:app}}"

[apps.acme-frontend]
depends_on = ["billing", "accounts"]
```

- `include` picks the three `acme-*` repos as apps, and leaves
  `side-project` out.
- `strip_prefix` shortens their hostnames to `accounts.acme.localhost`,
  `billing.acme.localhost` and `frontend.acme.localhost`.
- `[env]` gives every app the others' URLs, like
  `ACME_BILLING_URL=https://billing.acme.localhost`.
- `depends_on` starts billing and accounts along with the frontend.

A repo can also carry its own defaults in a `carte.toml`.

[docs/configuration.md](docs/configuration.md) describes every setting.

## Worktrees and stacks

Worktrees with the same folder name form a stack: their apps run
together and call each other. Say you add a worktree to billing:

```sh
cd ~/work/acme-billing
git worktree add ~/worktrees/acme-billing/feature-x -b feature-x
```

Now `https://billing.feature-x.acme.localhost` runs the worktree. The
frontend has no `feature-x` worktree, so
`https://frontend.feature-x.acme.localhost` runs its main checkout,
calling the `feature-x` billing. Remove the worktree, and carte stops it.

carte leaves databases alone. For separate data per worktree, use
`CARTE_CHECKOUT` in `database.yml` or similar.

## Day to day

Once a project is added, there's little left to do. Visiting an app
starts it, and it stops when you stop using it. New worktrees and
changes to project files are picked up by themselves.

```sh
carte restart   # the app you're in, to apply changes now
carte attach    # answer a debugger prompt; ctrl-q d detaches
carte           # open the terminal UI
```

`touch tmp/restart.txt` restarts an app too.

[docs/cli.md](docs/cli.md) covers the rest of the CLI. Coding agents can
use carte through its MCP server; see [docs/mcp.md](docs/mcp.md).

## Sharing

```sh
carte setup --share dev.example.com   # once, with Cloudflare Tunnel or ngrok
carte share billing@feature-x --auth --ttl 2h
```

Sharing is always explicit, and only shared apps are reachable. See
[docs/sharing.md](docs/sharing.md).

## Kubernetes

carte can forward to a Kubernetes cluster's Services, like
`kubectl port-forward` without the command. Give the cluster a project
file that names its kubeconfig context:

```toml
# ~/.config/carte/projects/prod.toml
kube = { context = "prod" }
```

The Service `grafana` in the `monitoring` namespace is then at
`https://grafana.monitoring.prod.localhost`, or
`https://grafana.prod.localhost` if no other namespace has a `grafana`.

`carte import kube` writes a project file for each context you pick. See
[Kube projects](docs/configuration.md#kube-projects).

## Configuration

Machine settings live in `~/.config/carte/config.toml`, and every key is
optional:

```toml
tld = "test"
idle_timeout = "15m"
```

carte reloads it when it changes. If a file is broken, carte keeps the
last working version and shows the error.

## Alternatives

carte is heavily inspired by [puma-dev](https://github.com/puma/puma-dev),
which is a good choice if you just want a web server per app.

[Overmind](https://github.com/DarthSim/overmind) and
[process-compose](https://github.com/F1bonacc1/process-compose) run a
project's processes in your terminal. They suit working on one app at a
time.

[portless](https://github.com/vercel-labs/portless) gives dev servers
`.localhost` names, but you start and stop them yourself.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. See [LICENSE](LICENSE).
