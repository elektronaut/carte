# Coming from puma-dev

carte grew out of [puma-dev](https://github.com/puma/puma-dev), and a
machine running it can switch over in two commands:

```sh
carte import puma-dev
carte setup
```

## What carries over

| puma-dev | carte |
| --- | --- |
| A link in `~/.puma-dev` | A project, named after the link |
| `myapp.test` | `myapp.test` if you keep `.test`, and `myapp.localhost` either way |
| Extra links to the same app | Aliases on the app |
| A port file | An app that routes to that port and runs nothing |
| `.env`, `.powenv`, `.pumaenv` | `env_files` |
| Starting on the first request | The same |
| Stopping after 15 minutes idle | The same; see `idle_timeout` |
| `touch tmp/restart.txt` | The same, except that creating the file doesn't count |

## What's different

- **The whole process group runs**, not only the web server. carte reads
  `Procfile.dev`, then `Procfile`, and falls back to
  `bundle exec puma -p $PORT` for a `config.ru`, as puma-dev does.
- **Env files are read, not run.** carte reads `KEY=VALUE` lines, with or
  without `export`, but runs no shell scripts. Move what `.powrc` or
  `~/.powconfig` sets into `env_files` or a project's `[env]`; the import
  points out each one it finds.
- **Creating `tmp/restart.txt` doesn't restart the app**; only touching
  a file that's already there does. Puma's `tmp_restart` plugin creates
  it when Puma first boots in a checkout, and that shouldn't restart the
  app. In a fresh checkout, touch it once more.
- **Worktrees get their own hostnames.** Link only the primary checkout:
  carte finds its worktrees by itself.
- **carte has its own certificate authority.** puma-dev's stays in place
  but isn't used.

## Importing

`carte import puma-dev` reads `~/.puma-dev` and writes a project file per
app into `~/.config/carte/projects/`. It prints each old hostname next to
its carte URL, then asks before writing. `--dry-run` only prints, and
`--from <dir>` reads another directory.

Each link becomes a project of one app, named after the link, so the app
keeps its hostname. Links to the same directory become aliases of one
app. When an alias can't be made, because the name is taken or isn't a
valid hostname, the import says so.

Some links are skipped, each with the reason:

- dangling links
- links to linked worktrees, since carte finds those from the primary
  checkout
- links to directories that aren't the root of a git repo
- port files that point at another machine or at `https`
- apps already in a carte project

Existing project files are never overwritten.

## Setting up

`carte setup` notices puma-dev and, when you have no carte projects yet,
suggests importing first. Its plan then:

- stops puma-dev's LaunchAgent and keeps it from starting at login, so
  carte can take over ports 80 and 443
- asks whether to keep puma-dev's `.test` hostnames, so bookmarks,
  `.env` URLs and OAuth callbacks keep working, or to switch to
  `.localhost`, which needs no resolver. `--tld test` or
  `--tld localhost` answers up front.
- to keep `.test`, points `/etc/resolver/test` at carte's DNS server;
  otherwise it removes puma-dev's resolver files

Apps answer under `.localhost` either way.

`~/.puma-dev`, puma-dev's LaunchAgent plist and its certificate authority
are left alone. A plist left behind after puma-dev itself was uninstalled
doesn't count as puma-dev: setup doesn't touch it, and `carte setup --check`
lists it as left behind.

## Going back

`carte setup --uninstall` removes carte's LaunchAgent, resolver files and
certificate authority. It removes only the CAs this install created, so
other users' carte CAs stay trusted. It then prints the commands that bring puma-dev
back: re-enabling its LaunchAgent, and `sudo puma-dev -setup` for its
resolver files. Your carte projects stay in `~/.config/carte`.
