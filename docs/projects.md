# Projects

A project is a group of apps that belong together, described by one file
in `~/.config/carte/projects/`. This page covers that file: where carte
finds a project's apps, templates that make a project per directory, how
names and hostnames are built, and the settings each app takes.

## Project files

A project file is `~/.config/carte/projects/<name>.toml`. The file name
is the project's name, and appears in hostnames. It takes at least three
characters: lowercase letters, digits and dashes.

You can sort project files into folders:

- A file in a subdirectory, like `projects/clients/shop.toml`, appears
  in that folder in the terminal UI. The folder doesn't change its name
  or hostnames.
- Folders can nest. `projects/clients/sites/nova.toml` appears in a
  `sites/` folder inside `clients/`.
- Names must be unique across all folders, since they are hostnames.
- Directories starting with a dot are skipped.

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
| `root` | none | The directory `include` entries and app paths are relative to, starting with `/` or `~/`. Without `include`, the root itself is the project's one app |
| `include` | the root | Where the apps are: a path or glob, or a list of them. See [Finding apps](#finding-apps) |
| `projects` | none | Makes the file a template for a project per directory. See [Templates](#templates) |
| `strip_prefix` | none | Removed from directory names to get the short names used in hostnames |
| `env_provider` | detected | Where the base environment comes from: `mise`, `direnv`, `shell` or `none`. See [Environment](environment.md) |
| `env` | none | Env templates for every app. See [Env templates](environment.md#env-templates) |
| `requires` | none | Services that must be reachable before anything in the project starts: `tcp://host:port` or `unix:///path` |
| `scheme` | `"https"` | The scheme of URLs carte injects. `"http"` for apps that misbehave behind HTTPS |
| `idle_timeout` | machine's | Overrides the machine's idle timeout for this project |
| `teardown_delay` | machine's | Overrides the machine's teardown delay for this project |
| `serve_files` | none | The default [`serve_files`](#serving-files) of the project's apps |
| `apps` | none | Per-app settings, keyed by app name. See [Apps](#apps) |
| `processes` | none | Processes declared for many apps at once. See [Project-level processes](processes.md#project-level-processes) |

carte resolves symlinks in `root`, in absolute `include` entries and in
app paths, so it finds the apps from either path.

### Finding apps

`include` entries are paths and globs. They are relative to `root`
unless they start with `~` or `/`. Each directory they match is an app:

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
- **A glob only picks directories with something to run.** That's a
  `Procfile.dev`, `Procfile` or `config.ru`, a `package.json` with a
  `dev` script, or a `carte.toml` with processes or `serve_files`. In a
  template, the project's `serve_files` folder counts too. Other
  directories next to your apps are left out.
- **A path without a glob is always an app.**
- **In a JS workspace,** libraries have `dev` scripts too. Match the
  apps' folder (`apps/*`), or leave a library out with `!`.
- **Globs match one directory level each.** `**` isn't supported.
- **Matches are looked for in every checkout.** A pattern that reaches
  into a git repo is also matched in each of the repo's worktrees. A
  stack has exactly the apps its worktree has. An app added on a branch
  exists only in that stack, and one deleted on a branch is gone from
  it. Asking for such an app where it doesn't exist says which stacks
  have it.
- **Apps in other repos** that have no worktree in a stack run from
  their main checkout there.
- **Directories outside git** are apps too, without worktrees. They run
  from their directory in every stack.
- **Linked worktrees** that a glob reaches are skipped. They belong to
  their repo's stacks.
- **New directories and repos** that match a glob join the project as
  they appear. An entry that matches nothing is reported.

A repo can name its own apps in the `carte.toml` at its root, so the
project file only needs to point at it. This applies when `include`, or
the implied root, picks a repo whose `carte.toml` has `include`, or
`[apps.X]` tables with a `path`. Those are then the repo's apps,
relative to the repo, instead of the repo itself:

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
take `path`. An app's settings go in the `carte.toml` in its own
directory.

An app that `include` finds is named after its directory. To give one a
different name, or to add an app `include` doesn't find, name it with
`path`:

```toml
[apps.billing]
path = "acme-billing"
```

The table's key is then the app's name. The directory belongs to that
app even when a glob also matches it.

Renaming an app changes its hostnames and its state directory.
`carte prune` cleans up the old one.

### Templates

A project file with `projects` is a template. Each directory its entries
match becomes a project of its own:

- The project is named after the directory.
- The directory is its `root`.
- Every other key in the template applies to it.

```toml
# ~/.config/carte/projects/clients/sites.toml
root = "~/clients/sites"
projects = ["*", "!archived-*"]
idle_timeout = "30m"

[apps.wiki]
aliases = ["old-wiki"]
```

With no `include`, each project's root is its one app, so
`~/clients/sites/wiki` is served at `wiki.localhost`. With
`include = "apps/*"`, each matched monorepo becomes a project with those
apps.

- **`projects` entries work like `include` entries.** They are paths and
  globs relative to `root`, apply in order, and use `!` to leave matches
  out.
- **A directory a glob matches** only becomes a project when it has an
  app with something to run. A path without a glob always does.
- **New directories** join as they appear.
- **`[apps.<name>]` tables** apply in every project that has an app of
  that name. One that matches no app in any of them is reported.
- **A project file with the same name** as a generated project wins, and
  replaces it entirely.
- **The generated projects appear in the template's folder** in the
  terminal UI. That's `clients` above, or a nested one like
  `clients/sites` for a template at `projects/clients/sites/static.toml`.
- **The template's own file name** only identifies it.
  `carte project edit sites` opens it, and `carte project ls` shows
  which projects came from it.
- **Directory names that aren't valid project names** are reported, and
  so are names that two templates both generate.

### Names and hostnames

An app's name is one of these:

- its directory name
- the key of the `[apps.X]` table that names it with `path`
- the project's name, for the project's root

Its short name is the name with `strip_prefix` removed, for apps named
after their directory. Short names must be at least three characters and
unique within the project.

Hostnames are built from short names, the stack and the project:

| | Hostname |
| --- | --- |
| On main | `billing.acme.localhost` |
| In a stack | `billing.feature-x.acme.localhost` |
| An app whose short name is the project's | `acme.localhost`, `feature-x.acme.localhost` |
| Shared | `billing--feature-x--acme.<share domain>` |

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
- A folder with the repo's own name gives way to the folder above it.
- Worktrees of different repos with the same name form one stack.
- Worktrees with a detached HEAD count like any other.
- Moving or renaming a worktree's folder (`git worktree move`) gives it
  a new stack name. The old stack is [torn down](processes.md#teardown).

carte reports a problem when two worktrees of a repo have the same name.
It also reports a worktree named `main`, and skips it.

A stack can't be named like an app of its project, in full or cut. The
app wins, and the stack is reported as unreachable.

#### Long stack names

A hostname label holds at most 63 characters. A stack name longer than
that is cut, and ends in a short hash of the full name.

A hostname holds at most 253 characters. When the project's longest
hostname would pass that, carte cuts long stack names in hostnames the
same way. This only happens when the app, stack and project names are
all long.

- When an app has `subdomains = true`, the name is cut further. This
  keeps room for one subdomain of up to 63 characters.
- The name is never cut below 8 characters: one character, a dash and
  the hash. Then only shorter subdomains fit.
- When even 8 characters don't fit, the name is left whole and a problem
  is reported.
- The stack keeps its full name everywhere else, such as in
  `CARTE_STACK`.

How far the name is cut depends on the project's apps. Adding an app
with a long name, or turning `subdomains` on or off, can change the
hostnames of every stack with a long name. Bookmarks, cookies and OAuth
callback URLs for the old hostname then stop working, and it drops out
of `RAILS_DEVELOPMENT_HOSTS`.

carte reports a problem when:

- a hostname doesn't fit even with the name cut
- an app with `subdomains = true` has room for only shorter subdomains
- a cut name is an app's label or the name of another stack. The app
  wins over a stack, and a stack wins over the cut name
- a label of the `tld` is longer than 63 characters

## Apps

App settings go in `[apps.<name>]` tables in a project file, or at the
top level of an app's `carte.toml`. When both set a key, the project
file wins, key by key. `env` and `processes` are merged entry by entry.
The project's [`[processes]`](processes.md#project-level-processes) sit
between the two.

`carte.toml` is read in each checkout, so a branch can change how its
app runs. In a worktree, that worktree's `env`, `env_files`, `processes`
and `replace_processes` apply. The other keys (`aliases`, `subdomains`,
`depends_on`, `policy`, `port`, `warm`) shape the whole project, so they
come from the main checkout's file.

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
| `env_files` | none | Files of `KEY=VALUE` lines, relative to the checkout, read in order. See [Env files](environment.md#env-files) |
| `depends_on` | none | Apps this app calls. See below |
| `aliases` | none | Extra hostnames routed to the app on main. See below |
| `subdomains` | `false` | Pass extra labels on the left (`oslo.events.acme.localhost`) through to the app instead of returning 404 |
| `policy` | `"auto"` | Default policy for the app's processes. See [Policies](processes.md#policies) |
| `port` | none | Route to a fixed port instead of running processes, for an app you start yourself |
| `serve_files` | none | A folder in the checkout, like `"public"`, whose files carte serves ahead of the app. See [Serving files](#serving-files) |
| `warm` | `true` | `false` keeps carte from starting the app ahead of a request. See [Learned warming](#learned-warming) |
| `replace_processes` | `false` | Ignore the Procfile and run only the processes configured here |
| `processes` | none | Per-process settings. See [Processes](processes.md) |

**`depends_on`** starts the listed apps together with this one, in the
same stack, and keeps them running while it runs.

- Name apps of the same project by their full or short name.
- Name an app of another project by its project, when that project has
  one app or an app named like the project. Otherwise, name it as
  `app.project`.
- Apps of another project always run on their project's main stack.

**`aliases`** are extra hostnames for the app on main. An alias is one
label without the TLD: `shop` serves `shop.localhost`.

- An alias equal to a project's name is skipped and reported.
- So is an alias equal to an alias of a project loaded earlier. An alias
  in a template therefore applies to only one of its projects.
- `carte import puma-dev` sets the old puma-dev names.
- For Rails apps, see `CARTE_TLD_LENGTH` under
  [Environment](environment.md).

### Serving files

With `serve_files`, carte answers GET and HEAD requests for files in
that folder itself. Everything else goes on to the app. This is how
puma-dev serves `public/`:

```toml
[apps.shop]
serve_files = "public"
```

- **A path is looked up like Rails does.** `/about` tries `about`,
  `about.html` and `about/index.html`, so `/` serves `index.html`.
- **A file is served without starting the app,** so assets and error
  pages load while it boots or after it crashes.
- **An app with `serve_files` and nothing to run is a static site.**
  carte serves its files, and a path with no file gets a 404.
- **Each worktree serves the folder in its own checkout.**
- **Dotfiles and links that lead outside the folder are never served,**
  also when the app is shared. `.well-known` is the exception.

The folder is relative to the checkout, and `"."` serves the checkout
itself. Set `serve_files` at the top of a project file or template to
give every app in it the same folder.

### Learned warming

carte learns which apps you open after which, and starts them ahead of
time.

An app is opened by its first request after it starts. Say you open
`billing` within two minutes of `shop`, in the same stack. carte then
notes that `billing` followed that open of `shop`. A request to
`billing` from a page of `shop`, by its `Origin` or `Referer`, counts
too, however long after.

carte keeps each app's last 10 opens. When `billing` followed at least
half of them, and at least 3, opening `shop` starts `billing` in the
same stack. What's learned on main applies to new worktrees right away.

A warmed app is an ordinary start:

- It idles out as usual, and counts toward the
  [limits](configuration.md#limits).
- Nothing is warmed while memory or the instance count is over its
  target.
- A warmed app that idles out without a request counts against the
  guess, so guesses that don't help fade.
- Nothing fades while you don't use an app.

`carte status shop` shows what it warms, as `billing (8/10)`.
`carte events` says why a warmed app started. What carte has learned is
kept in `warming.json` in the state directory. `warm = false` keeps an
app from being warmed.

### Requests from other sites

A page on another site can't start an app. Say it makes a request for
an app that isn't running, such as an image, a `fetch` or a frame. The
request gets a 403 page and leaves the app stopped. It doesn't open the
app or warm others.

carte decides whether a request is another site's like this:

- It looks at the page that made the request, by its `Origin` header.
  When the `Origin` is missing or hidden (`null`), it uses the
  `Referer` instead.
- A page outside carte's TLDs and loopback is another site.
- When neither header names a page, the request counts as another
  site's if its `Origin` is hidden, or if the browser marks it
  cross-site.

A local page that sends no referrer (`Referrer-Policy: no-referrer` or
`rel=noreferrer`) looks the same as another site to carte. Its images,
scripts, frames and form posts can't start an app the browser counts
as another site. Its `fetch` calls send their `Origin`, so they can.

Opening the app yourself starts it, also by a link from another site.
Requests from local pages start it too, and so do requests from clients
like `curl` that send no `Sec-Fetch-*` headers. Once the app runs,
requests from other sites reach it as usual, and its own CORS and CSRF
checks apply.
