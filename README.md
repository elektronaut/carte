# carte

Run your local web apps _à la carte_: visit `https://myapp.localhost` and
carte starts the app, then stops it once you're done with it.

- **Many apps at once:** add every app you work on, and carte keeps
  memory in check by stopping the least recently used first.
- **The whole process group**, not only the web server. Everything in
  `Procfile.dev` starts and stops together, including asset watchers,
  job workers and schedulers.
- **A hostname per worktree:** a worktree in a `feature-x` folder runs
  at `https://feature-x.myapp.localhost`.
- **Projects and stacks:** a project can consist of many apps, and
  their worktrees of the same name operate together as a stack.
- **HTTPS everywhere,** with a local certificate authority your apps and
  browsers trust.
- **A terminal UI** for state, logs and controls.
- **Sharing** apps on the public internet through Cloudflare Tunnel or
  ngrok.
- **Kubernetes Services** at hostnames like
  `https://grafana.monitoring.prod.localhost`, forwarded when you visit.

carte runs on macOS and Linux. On Linux, `carte setup` runs the daemon
as a systemd user service. A TLD other than `.localhost` also needs
systemd-resolved; without it, setup tells you what to configure instead.

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

- creates a local certificate authority that can sign only names under
  the TLDs carte serves, and trusts it for HTTPS only
- installs a LaunchAgent that runs the daemon on ports 80 and 443, on
  127.0.0.1 and ::1
- on Linux, installs systemd user units in its place: a socket unit for
  each port and a service that runs the daemon. Ports below 1024 need
  root there, so setup also lowers `net.ipv4.ip_unprivileged_port_start`
  to 80 in `/etc/sysctl.d/50-carte.conf`, which applies to every program
  on the machine
- on Linux, adds the certificate authority to the system's certificates
  (`update-ca-certificates` or `update-ca-trust`) and, with `certutil`,
  to the databases Chrome and each Firefox profile read. Snap and
  Flatpak browsers keep their own; setup names them for you to import
  it by hand
- for a TLD other than `.localhost`, sends its lookups to carte: through
  `/etc/resolver/<tld>` on macOS, and on Linux through a systemd-resolved
  DNS delegate (systemd 258 and later) or, on older versions where it
  leaves other lookups alone, a drop-in. Where neither fits, setup says
  what to configure, such as a dnsmasq line

Apps are served under `.localhost`, which needs no DNS setup;
`carte setup --tld test` adds `.test`. `carte setup --check` reports
what's in place, and `carte setup --uninstall` undoes it, removing only
the CAs this install created from the keychain or the Linux stores.

If you set carte up before its CA was limited to HTTPS, run `carte
setup` once after upgrading. It replaces the CA with one trusted for
HTTPS only, and removes the old one.

### Coming from puma-dev

`carte setup` will offer to replace any existing puma-dev installation.
If you wish to keep using the `.test` hostname, a new resolver will be
installed.

To import your linked apps in `~/.puma-dev` to carte projects, you can run:

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

carte runs the processes in `Procfile.dev`, or `Procfile`, and gives each
one its own `PORT`. Apps without a Procfile work too: a Rack app runs
with Puma, and a Node app runs its `dev` script. A dev server next to
Rails, like vite_ruby's or Shakapacker's, gets its paths on the app's
hostname; see [Paths](docs/configuration.md#paths).

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
- `[env]` gives every app a variable per app, like
  `ACME_BILLING_URL=https://billing.acme.localhost`, so the frontend
  knows where to find the APIs.
- `depends_on` starts billing and accounts along with the frontend.

A repo can also carry its own defaults in a `carte.toml`.

[docs/configuration.md](docs/configuration.md) describes every setting:
monorepos, templates for a folder of unrelated apps, services and tasks,
teardown, serving files, and how each process's environment is built.
`carte env` shows that environment, with where each value comes from.

## Worktrees and stacks

A stack is a set of apps that run and call each other together. You don't
create stacks yourself: each worktree joins the stack named after its
folder, so worktrees of several repos with the same folder name end up in
one stack. Adding a worktree to billing:

```sh
cd ~/work/acme-billing
git worktree add ~/worktrees/acme-billing/feature-x -b feature-x
```

Now `https://billing.feature-x.acme.localhost` runs the worktree, and
`https://frontend.feature-x.acme.localhost` runs the frontend's main checkout
as a separate instance whose URLs point into the `feature-x` stack. When
you remove the worktree, carte stops its instance and the stack falls
back to main.

carte doesn't touch databases. Use `CARTE_CHECKOUT` in `database.yml` or
similar if you want separate data per worktree.

[Names and hostnames](docs/configuration.md#names-and-hostnames) has the
naming rules.

## Day to day

Once a project is added, there's little left to do. Visiting an app
starts it, new worktrees get their hostnames as you create them, and
apps stop when you stop using them. carte picks up changes to project
files by itself, and running apps get them on their next start.

What's left:

```sh
carte restart   # the app you're in, to apply changes now
carte attach    # answer a debugger prompt; ctrl-q d detaches
carte           # open the terminal UI
```

`touch tmp/restart.txt` restarts an app too.

The CLI also starts, stops, pins and inspects apps, and every command
takes `--json`; see [docs/cli.md](docs/cli.md). Coding agents can use
carte through `carte mcp`, an MCP server; see [docs/mcp.md](docs/mcp.md).

## Sharing

```sh
carte setup --share dev.example.com   # once, with Cloudflare Tunnel or ngrok
carte share billing@feature-x --auth --ttl 2h
```

Sharing is always explicit, and only shared apps are reachable. See
[docs/sharing.md](docs/sharing.md).

## Kubernetes

carte can also reach the Services of a Kubernetes cluster, the way
`kubectl port-forward` does, but with nothing to run first. Give each
cluster a project file that names its kubeconfig context:

```toml
# ~/.config/carte/projects/prod.toml
kube = { context = "prod" }
```

The Service `grafana` in the `monitoring` namespace is then at
`https://grafana.monitoring.prod.localhost`. You can leave out the
namespace, as in `https://grafana.prod.localhost`, as long as only one
namespace has a Service of that name.

`carte import kube` writes a project file for each context you pick. See
[Kube projects](docs/configuration.md#kube-projects).

## Configuration

Machine settings live in `~/.config/carte/config.toml`, and every key is
optional:

```toml
tld = "test"
idle_timeout = "15m"
```

carte reloads configuration when files change. A broken file keeps the
last working version, and `carte ls` and the terminal UI show the error.

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

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. See [LICENSE](LICENSE).
