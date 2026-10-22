# Terminal UI

`carte`, or `carte tui`, opens the terminal UI. It shows every project,
stack, app and process the daemon knows, with their state, logs and
events, and lets you start, stop, share and inspect them.

It opens on the app or stack whose checkout holds the current
directory. While the daemon isn't reachable, it says so and retries
every second.

## Panes

The tree is on the left. The selected row's details and tabs are on
the right, the details above the tabs. A status line and a line of
keys sit below them. In a terminal narrower than 110 columns, the tree
is on top instead.

`tab` moves the focus between the tree and the tabs. The focused
pane's border has the theme's focus colour, blue in the `terminal`
theme. The keys at the bottom are the most used ones for the focused
pane. While there are problems, or the share tunnel is down, the keys
start with `! problems`.

`z` zooms the right side: the details and tabs fill the space, with
the details at full width on top. Their short facts then share a line.
`z` goes back to the focus you had. So does `esc`, once the search is
clear. `tab` goes back to the tree. The tree doesn't zoom.

### Tree

The tree groups folders, projects, stacks, apps and processes. Inside a
project, a level with a single child merges into its parent: a project
with only the main stack lists its apps directly, and a single-app
project shows the app on the project's row. An app with a single
process has no process row. Folders never merge, and nothing merges
inside a [kube project](kubernetes.md).

The title counts running instances, shares and problems. Clicking the
problem count, or `tunnel down`, lists the problems. The `CPU` and `MEM` columns cover
everything running below a row.

Each row starts with a symbol for its state:

| Symbol | State |
| --- | --- |
| `●` | Running |
| `◐` | Starting or restarting |
| `◑` | Stopping |
| `◆` (magenta) | Paused at a debugger prompt |
| `◆` (yellow) | Suspended by a signal |
| `○` | Stopped, or exited cleanly |
| `✓` | A task that finished |
| `✖` | Crashed |
| `⊘` | Disabled |
| `?` | Unknown |

Apps and processes can carry badges after their name:

| Badge | Means |
| --- | --- |
| `main` | The stack runs this app from the main checkout |
| `pin` | Pinned: carte doesn't stop it for idleness |
| `off` | Disabled |
| `pin*`, `off*` | Temporary: lasts until the daemon restarts |
| `shared` | On the public internet; see [sharing](sharing.md) |
| `gave up` | carte stopped restarting a crashing process |
| `!` | Has a problem; `!` lists them |

### Details and tabs

The details show the selected row's state and what needs attention:
its address and share, boot progress, what keeps it running, pins and
disables that last until the daemon restarts, a process's pid,
restarts and exit, the last error, its latest event and its problems.
They take five lines, so the tabs below stay put as you move, zoomed
or not. When more is going on, errors and problems come right after
the state, a long line ends in `…`, and the last line says how many
more there are. The config tab has the rest.

Below the details, the pane's top border lists its tabs: `1 logs`,
`2 events`, `3 env`, `4 config` and `5 requests`. The current tab is
highlighted. A tab that doesn't apply to the selection, such as env on
a folder, is dimmed. `1` to `5` pick a tab, and `[` and `]` go to the
previous and next.
Picking a tab doesn't move the focus, so you can move through the tree
and watch each row's tab.

Each tab shows the selection. With nothing selected, or a folder,
events and requests are machine-wide.

The logs of a project, stack or app merge the logs of its processes.
Each line starts with its process's name in a colour of its own.

The events are the selection's starts, stops, crashes and the like.

The logs and events follow new lines. Scrolling up stops that, and the
border shows how far up you are. `G` follows again.

The env tab shows a process's env: each variable, its value and where
it came from. Values aren't masked. On an app, it shows the env of the
process that serves HTTP, and says so; select a process to see
another's. An app with several processes and none serving HTTP lists
them to pick from. `3` on the env tab fetches it again, as the env can
change when a process starts.

The config tab shows what carte resolved for the selection, from its
configuration only. On an app: its checkout, hostnames, aliases, env
sources, idle timeout, dependencies, served files, and each process
with its command. On a process: its command, the command as it runs
with its port, the port, readiness, paths, how it stops and restarts,
and its policy. On a project or stack: the project file, settings, and
each stack's checkouts. On a [kube project](kubernetes.md): its
context, namespaces and forwards. Values someone configured are bold:
those that aren't carte's default, or aren't what the machine, the
project or the app gives them. Defaults are plain. A pin or disable
that lasts until the daemon restarts shows apart from the configured
policy. `4` on the config tab fetches it again.

The requests tab lists the requests carte's proxy answered: the time,
method, path, status, and how long it took. That is how long carte held
it, waiting for a boot included, plus how long the app took to answer;
for a page carte answered itself, it's the hold alone. A request whose
client went away first says `cancelled`. The tab starts with the latest
200 requests and keeps up to 1000. Moving through the tree, it waits a
moment for the selection to settle before it asks for the next. `↗` marks a
request that came through a share. New requests follow at the bottom.
`↑` selects a request and stops that; `G` follows again.

`enter` opens the selected request, or the newest: the request line
and status, the host, which app and process answered and from which
checkout, whether the host was an alias, apps it warmed, how long carte
held it and how long the app took, the visitor of a shared request, and
the request and response headers in full. carte keeps no bodies, and
cuts very long headers short; the request says when it did. `esc` goes
back to the list.

Zoomed with `z`, the requests tab shows the list on the left and the
selected request, or the newest, on the right. The wheel over the
request scrolls it.

## Show filter

`a` cycles which instances the tree shows:

| Show | Keeps |
| --- | --- |
| All | Every instance |
| Running | Instances that aren't stopped, exited, done or disabled |
| Shared | Shared instances |

The tree's title names the filter while it isn't `all`. The processes
of a shown app always show.

## Filter and search

`/` in the tree filters it by name. A match keeps its subtree, and its
parents open to show it. `/` in the logs searches them: matches are
highlighted, and `n` and `N` move to the next older and newer match.
`ctrl-f` while you type switches the logs to a filter, which hides the
lines without a match, and back. The tree only filters. The search
works in the logs and events tabs, not the env.

You type on a line at the bottom of the pane. In the logs, the line
counts the matches. `enter` keeps the term, and the line stays while
it's kept. `esc` clears the one in the focused pane.

`ctrl-p` jumps to a project, stack, app or process by name, wherever it
is in the tree. It focuses the tree and opens the folds above the row.
If the filter or the show filter hides the row, it clears both.

## Folds

Projects start open, and everything below them starts closed. A
single-app project shows its app on its row, so it starts closed like
an app. Folders start closed. A [kube project](kubernetes.md) starts closed, since
opening it asks the cluster for its Services; its namespaces start open
while something in them runs or failed.

`←` and `→`, or `h` and `l`, close and open the selected row. `←` on a
closed row moves to its parent, and `→` on an open row moves into it.
`enter` and `space` toggle it.

## Keys

`?` shows the main keys. They work in either pane unless noted.

### Navigate

| Key | Does |
| --- | --- |
| `↑` `↓`, `j` `k` | Move in the tree, or scroll the tab |
| `←` `→`, `h` `l` | Close, open the selected row (tree). `←` `h` in the tabs moves to the tree |
| `enter`, `space` | Close or open the selected row (tree) |
| `g` `G`, `Home` `End` | Top, bottom. `G` in the logs and requests follows new ones |
| `PgUp` `PgDn` | Page up, down |
| `tab` | Switch between the tree and the tabs |
| `1` to `5` | Show the logs, events, env, config or requests tab |
| `[` `]` | Previous, next tab |
| `enter`, `esc` | Open a request, back to the list (requests) |
| `/` | Filter the tree, or search the logs |
| `ctrl-f` | Filter or search the logs, while you type |
| `n` `N` | Older, newer match (logs) |
| `esc` | Clear the filter or search of the focused pane, or leave the zoom |
| `a` | Show all, running or shared instances |
| `z` | Zoom the details and tabs, or go back |
| `-` | Mark the logs with the time |
| `m` | Mouse on or off |
| `ctrl-p` | Jump to a project, stack, app or process |

### Control the selection

| Key | Does |
| --- | --- |
| `s` | Start |
| `x` | Stop |
| `r` | Restart |
| `p` | Pin or unpin |
| `d` | Disable or enable |
| `S` | Save a process's temporary pin or disable to the project file |
| `o` | Open in the browser |
| `w` | Share on the web, or stop sharing |
| `W` | Share behind a password |
| `O` | Open the shared address |
| `y` `Y` | Copy the address, the shared address |
| `t` | Type into the process, like at a debugger prompt |

`S` acts on the selected process, or on the one process below the
selection with a temporary pin or disable.

`t` types into the selected process. On an app, it picks the paused
process, the web process, or the only process. Every key goes to the process until the
[detach keys](cli.md#attach), `ctrl-q d` by default. The status line
shows the prompt and what you've typed.

### Everything

| Key | Does |
| --- | --- |
| `I` | Stop idle instances |
| `X` | Stop all instances, after asking |
| `!` | List problems |
| `T` | Pick a colour [theme](#themes) |
| `?` | Show the keys |
| `q`, `ctrl-c` | Quit |

Any key closes the help and the problems.

## Mouse

| Action | Does |
| --- | --- |
| Click a row | Select it |
| Click the selected row | Close or open it |
| Click the problem count or `tunnel down` | List the problems |
| Click a pane | Focus it |
| Click a tab's name | Show that tab |
| Click a request | Select it |
| Wheel over the tree | Move the selection |
| Wheel over the tabs | Scroll the current tab |
| Drag over the logs | Select lines; releasing copies them |

`m` turns the mouse off, so the terminal's own selection works, and on
again.

## Themes

`T` lists the colour themes, with a `•` next to the current one and
the highlighted one drawn like a selected row. Moving the highlight
shows each theme on the whole screen at once.
`enter` keeps it. `esc`, `q` and `T` go back to the theme you had, and
`ctrl-c` goes back to it and quits.

| Theme | Kind |
| --- | --- |
| Terminal | Your terminal's own colours; the default |
| Catppuccin Latte | Light |
| Catppuccin Frappé, Macchiato, Mocha | Dark |
| Tokyo Night | Dark |
| Tokyo Night Day | Light |
| Gruvbox Dark, Gruvbox Light | Dark, light |
| Dracula | Dark |
| Alucard | Light |
| Nord | Dark |
| Solarized Dark, Solarized Light | Dark, light |
| Rosé Pine | Dark |
| Rosé Pine Dawn | Light |

Every theme but `terminal` paints its own background, so a light theme
stays readable in a dark terminal. They use 24-bit colour when
`COLORTERM` is `truecolor` or `24bit`, and the nearest of the 256
colours otherwise. Over SSH, `COLORTERM` often isn't passed on.

Each theme follows its authors' palette, with a few colours adjusted so
they read on the theme's background and on the selection:

- Text has a contrast of at least 4.5:1.
- Status colours, such as running, starting and crashed, the accent
  and process names have at least 3:1. They are darkened or lightened
  by at most half the way to black or white, which keeps their hue.
- Secondary text has at least 3:1. It moves toward the text colour, by
  at most 60% of the way.
- Search matches have at least 4.5:1 against their background, which
  is darkened or lightened away from the text.
- The focused pane's border has at least 3:1 against the background
  and 1.5:1 against the other borders, which move toward the
  background where needed.

At 256 colours, each colour becomes the closest of the 256 that keeps
the same contrast. Colours stay colours, greys stay greys, and the
status colours stay apart, but a hue can shift a little, since the
256 colours have few shades of each.

`terminal` draws selected log lines and the jump prompt's pick
reversed, since its colours are up to your terminal.

### Custom themes

A file in `~/.config/carte/themes` adds a theme. The folder is in
carte's config directory, so `CARTE_CONFIG_DIR` or `XDG_CONFIG_HOME`
moves it (see [configuration](configuration.md)). A file's name,
without `.toml`, is the theme's id, and the theme appears in the `T`
list after the built-ins. A file named like a built-in, such as
`nord.toml`, replaces it. Files whose names start with `.` are left
out.

```toml
# ~/.config/carte/themes/nova.toml
extends = "catppuccin-mocha"
label = "Nova"
accent = "#f5a97f"
muted = "bright-black"
prefixes = ["#8bd5ca", "#c6a0f6", "#eed49f", "#8aadf4", "#f5bde6", "#a6da95"]
```

`extends` names a built-in or another custom theme. Roles the file
leaves out come from it, or from `terminal` without `extends`. A file
that extends its own id, like `extends = "nord"` in `nord.toml`, builds
on the built-in. `label` is the name in the list. It defaults to the
built-in's label for a file that replaces one, and to the id
otherwise.

The built-in ids are `terminal`, `catppuccin-latte`,
`catppuccin-frappe`, `catppuccin-macchiato`, `catppuccin-mocha`,
`tokyonight-night`, `tokyonight-day`, `gruvbox-dark`, `gruvbox-light`,
`dracula`, `alucard`, `nord`, `solarized-dark`, `solarized-light`,
`rose-pine` and `rose-pine-dawn`.

Every role is optional:

| Role | Colours |
| --- | --- |
| `fg` | Text |
| `bg` | The background of the whole screen |
| `muted` | Secondary text, fold markers, stopped and disabled things |
| `accent` | Keys in the help and the key line, stacks, pins |
| `border` | Pane borders |
| `border_focus` | The focused pane's border |
| `ok` | Running and ready, notices |
| `warn` | Starting, stopping, suspended, scroll positions, prompts |
| `error` | Crashes, problems, errors |
| `shared` | Shares and the tunnel |
| `paused` | A process at a debugger prompt |
| `selection_bg` | The selected tree row, selected log lines and the jump prompt's pick |
| `selection_fg` | Secondary text on the selected tree row |
| `match_fg`, `match_bg` | Search matches in the logs |
| `match_current_fg`, `match_current_bg` | Matches on the current match's line |
| `prefixes` | Process names in merged logs, a list used in turn |

A colour is one of:

- `"#rrggbb"`.
- An ANSI name: `black`, `red`, `green`, `yellow`, `blue`, `magenta`,
  `cyan`, `gray`, and `bright-black` to `bright-white`. `gray` is ANSI
  7, and `white` is the same as `bright-white`, ANSI 15. Your terminal
  decides how these look.
- A number from 0 to 255, for the 256-colour palette.
- `"reset"`. In `fg` and `bg`, it's the terminal's own colour. In any
  other role, it inherits: text takes `fg`, and a background leaves what
  is under it. In a theme with a `bg`, `selection_bg = "reset"` makes
  the selected tree row look like the others.

carte doesn't check or adjust custom themes for contrast, as it does
the built-ins; the colours are drawn as written. At 256 colours, each one
becomes the nearest of the 256. Roles a theme inherits from a built-in
keep the built-in's adjustments.

carte watches the folder, so a saved file shows at once, and redraws
if it's the current theme. A file that doesn't parse, sets an unknown
role or colour, or extends something that isn't a theme, including in
a cycle, shows under `!` with its file and line. A file that extends
it shows there too. Its theme is left out of the list, or for a file
that replaces a built-in, the list has the built-in. If it's the
current theme, the TUI keeps drawing the theme as it last loaded. A
theme saved in `tui.json` whose file doesn't load is used as soon as
the file is fixed.

## Saved state

The terminal UI remembers some choices between sessions, in `tui.json`
in carte's state directory, `~/.local/state/carte` by default (see
[configuration](configuration.md)). It saves them when you quit.

- The show filter.
- The colour theme.
- Rows you opened or closed. Rows that no longer exist are dropped.

A kube project's own fold isn't saved, so it opens closed each time.
The tree filter, the logs search and the focus start fresh.
