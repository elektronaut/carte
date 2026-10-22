# Configuration

carte reads three kinds of TOML files:

| File | Holds |
| --- | --- |
| `~/.config/carte/config.toml` | Machine settings: TLD, ports, idle timeout, limits, sharing |
| `~/.config/carte/projects/<name>.toml` | One project: where its apps are, env and per-app settings. Or a [kube project](kubernetes.md) |
| `carte.toml` in a repo | Defaults for that app, which can be committed |

The config directory is `~/.config/carte`. carte's state lives in
`~/.local/state/carte`: logs, the CA, pidfiles and instance data. When
`$XDG_CONFIG_HOME` and `$XDG_STATE_HOME` are set, the two directories
go under them instead.

`CARTE_CONFIG_DIR` and `CARTE_STATE_HOME` set the two directories
directly. `carte setup` writes both into the daemon's LaunchAgent, or
into its systemd user service on Linux.

Every key is optional unless noted. Unknown keys are errors, so a typo is
reported instead of ignored.

The daemon reloads files when they change. A file with an error keeps
its last working version, and `carte ls` and the terminal UI show the
error. Changes to `[share]` need a daemon restart
(`carte daemon restart`).

**Durations** are strings of numbers with the units `d`, `h`, `m`, `s`
and `ms`, like `"15m"`, `"1h30m"` or `"500ms"`.

**Sizes** are a number of bytes, or a string with a binary unit, like
`"512KB"`, `"256MB"` or `"1GB"`.

The other pages cover project files and what's in them:

- [Projects](projects.md): project files, finding apps, templates, names
  and hostnames, and per-app settings
- [Processes](processes.md): services and tasks, crashes, notifications,
  teardown, Node apps and policies
- [Dev servers](dev-servers.md): dev servers that serve paths on an
  app's hostname, like vite_ruby's or Shakapacker's
- [Environment](environment.md): how each process's environment is built,
  instance directories, env templates and env files
- [Kubernetes](kubernetes.md): kube projects and forwarding

## Machine config

The machine config is `~/.config/carte/config.toml`. Without the file,
every setting has its default.

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
| `tld` | `"localhost"` | The top-level domain apps are served under. See below |
| `http_port` | `80` | Plain HTTP |
| `https_port` | `443` | HTTPS |
| `dns_port` | `9253` | carte's DNS server, for a `tld` other than `localhost`. `/etc/resolver/<tld>` or systemd-resolved points to it |
| `idle_timeout` | `"15m"` | How long an app runs without traffic before it's stopped. Projects can override it. An app with a process at a [debugger prompt](cli.md#attach) doesn't stop |
| `teardown_delay` | `"10m"` | How long a stack's worktrees must stay gone before its [teardown](processes.md#teardown) runs. Projects can override it |
| `notifications` | `true` | Desktop [notifications](processes.md#notifications) when a process crashes, when carte gives up on it, or when it stops at a debugger prompt |

A `tld` other than `localhost`, like `"test"`, needs the system to send
its DNS lookups to carte. `carte setup` sets this up:

- On macOS, it writes `/etc/resolver/<tld>`.
- On Linux, it writes a systemd-resolved DNS delegate (systemd 258 and
  later) or a drop-in.

Apps then answer under `.localhost` too.

`carte setup` refuses a `tld` that isn't dot-separated letters, digits
and hyphens, such as `".test"` or a path. It keeps refusing until
`carte setup --tld <tld>` replaces it.

Changing `tld` or the ports means running `carte setup` again.
`carte setup --tld <tld>` changes `tld` and does the rest.

### `[limits]`

Limits are targets, not walls. They decide which idle apps to stop, and
never block a start.

| Key | Default | |
| --- | --- | --- |
| `memory_target` | `"50%"` | Above this, the least recently used apps are stopped. A percentage of RAM or a size (`"16GB"`) |
| `instance_target` | none | The same, for the number of running instances |
| `hard_ceiling` | `50` | No more instances than this start, and requests get an error instead. Catches runaway loops |
| `pty_reserve` | `64` | Ptys left free for your own terminals. Processes fall back to pipes rather than use them |
| `log_buffer` | `"1MB"` | Log kept per process |
| `log_budget` | `"256MB"` | Log kept in total. Stopped apps' logs go first |

carte never stops an app to meet a target while it is:

- starting, or became ready in the last 10 seconds
- pinned
- handling requests
- running a `carte exec` command
- kept running by an app that depends on it
- waiting at a [debugger prompt](cli.md#attach)

### `[share]`

`carte setup --share <domain>` sets up this section. Without it, sharing
is off.

| Key | Default | |
| --- | --- | --- |
| `provider` | `"cloudflare"` | `cloudflare`, `ngrok`, or `none` to run your own tunnel to the share port |
| `domain` | required | The domain shared apps are served under. For ngrok, your static domain |
| `tunnel` | `"carte"` | The Cloudflare tunnel's name |
| `port` | `9254` | The local port the tunnel delivers requests to |

### `[attach]`

These settings are for [`carte attach`](cli.md#attach), and for typing
into a process from the terminal UI.

| Key | Default | |
| --- | --- | --- |
| `detach_key` | `"ctrl-q d"` | The keys that detach. See below |

`detach_key` is a list of keys separated by spaces. Each key is
`ctrl-<key>` or a single character.

- One character alone isn't allowed.
- With two different keys, typing the first one twice sends it once.
- A letter after the prefix works on any keyboard layout, unlike `ctrl-]`
  or `~`.
