# Terminal UI

`carte`, or `carte tui`, opens the terminal UI. It shows every project,
stack, app and process the daemon knows, with their state, logs and
events, and lets you start, stop, share and inspect them.

It opens on the app or stack whose checkout holds the current
directory. While the daemon isn't reachable, it says so and retries
every second.

## Panes

The tree is on the left. The selected row's details and logs are on
the right, the details above the logs. A status line and a line of
keys sit below them. In a terminal narrower than 110 columns, the tree
is on top instead.

`tab` moves the focus between the tree and the logs. The focused pane
has a blue border. The keys at the bottom are the most used ones for
the focused pane.

### Tree

The tree groups folders, projects, stacks, apps and processes. Inside a
project, a level with a single child merges into its parent: a project
with only the main stack lists its apps directly, and a single-app
project shows the app on the project's row. An app with a single
process has no process row. Folders never merge, and nothing merges
inside a [kube project](kubernetes.md).

The title counts running instances, shares and problems. The `CPU` and
`MEM` columns cover everything running below a row.

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

### Details and logs

The details say what the selected row is doing, what keeps it running,
its address and share, and its latest event.

The logs of a project, stack or app merge the logs of its processes.
Each line starts with its process's name in a colour of its own. `e`
switches the pane to the selection's events, such as starts, stops and
crashes, and back.

The logs follow new lines. Scrolling up stops that, and the title shows
how far up you are. `G` follows again.

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
`enter` keeps the filter or search. `esc` clears the one in the
focused pane.

`ctrl-p` jumps to a project, stack, app or process by name, wherever it
is in the tree. It focuses the tree and opens the folds above the row.
If the filter or the show filter hides the row, it clears both.

## Folds

Projects start open, and everything below them starts closed. A
single-app project shows its app on its row, so it starts closed like
an app. Folders start closed. A [kube project](kubernetes.md) starts closed, since
opening it asks the cluster for its Services; its namespaces start open
while something in them runs.

`←` and `→`, or `h` and `l`, close and open the selected row. `←` on a
closed row moves to its parent, and `→` on an open row moves into it.
`enter` and `space` toggle it.

## Keys

`?` shows the main keys. They work in either pane unless noted.

### Navigate

| Key | Does |
| --- | --- |
| `↑` `↓`, `j` `k` | Move in the tree, or scroll the logs |
| `←` `→`, `h` `l` | Close, open the selected row (tree). `←` `h` in the logs moves to the tree |
| `enter`, `space` | Close or open the selected row (tree) |
| `g` `G`, `Home` `End` | Top, bottom. `G` in the logs follows new lines |
| `PgUp` `PgDn` | Page up, down |
| `tab` | Switch between the tree and the logs |
| `/` | Filter the tree, or search the logs |
| `n` `N` | Older, newer match (logs) |
| `esc` | Clear the filter or search of the focused pane |
| `a` | Show all, running or shared instances |
| `e` | Show logs or events |
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
| `v` | View the env |
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
| `?` | Show the keys |
| `q`, `ctrl-c` | Quit |

Any key closes the help and the problems. The env scrolls with the
navigation keys and `space`, and other keys close it.

## Mouse

| Action | Does |
| --- | --- |
| Click a row | Select it |
| Click the selected row | Close or open it |
| Click a pane | Focus it |
| Wheel over the tree | Move the selection |
| Wheel over the logs | Scroll them |
| Drag over the logs | Select lines; releasing copies them |

`m` turns the mouse off, so the terminal's own selection works, and on
again.

## Saved state

The terminal UI remembers some choices between sessions, in `tui.json`
in carte's state directory, `~/.local/state/carte` by default (see
[configuration](configuration.md)). It saves them when you quit.

- The show filter.
- Rows you opened or closed. Rows that no longer exist are dropped.

A kube project's own fold isn't saved, so it opens closed each time.
The tree filter, the logs search and the focus start fresh.
