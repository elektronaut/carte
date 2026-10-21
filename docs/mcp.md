# MCP server

`carte mcp` serves the [Model Context Protocol](https://modelcontextprotocol.io)
over stdio, so coding agents can check on and control the apps they work
on. It talks to the running daemon, the same way the CLI does.

The agent's client starts the server in the project it's working in, and
addresses are resolved from that directory like the CLI resolves them: with
no address, a tool means the app of the current checkout.

## Registering it

Claude Code:

```sh
claude mcp add carte -- carte mcp
claude mcp add carte -- carte mcp --control   # also start, restart and stop
```

Other clients take a command and arguments, usually in JSON:

```json
{
  "mcpServers": {
    "carte": { "command": "carte", "args": ["mcp"] }
  }
}
```

## Tools

Every tool takes an optional `address` (`app@stack/process`) and
`project`, as the CLI does. Results are the JSON that the CLI's `--json`
prints; logs and events come as one JSON object per line.

| Tool | Does | More arguments |
| --- | --- | --- |
| `status` | The state of an app, or the running apps of a stack or project, as `carte status` | |
| `logs` | Recent log lines, as `carte logs` | `lines`, `since` (`"5m"`), `since_start`, `search` |
| `logs_search` | The log lines most relevant to some words, as `carte logs --rank` | `query`, `top` |
| `events` | What carte did and why, as `carte events` | `limit` |
| `env` | A process's environment and where each value comes from, as `carte env`. Values from the environment and env files are always hidden | `process` |

With `--control`:

| Tool | Does |
| --- | --- |
| `start` | Starts an app, stack or process and waits until it's ready, as `carte up` |
| `restart` | Restarts and waits, as `carte restart` |
| `stop` | Stops, as `carte down` |
| `type` | Types `text` into a process's terminal, then enter unless `enter` is false, and returns what it printed once its output has been quiet for `quiet` (`"500ms"`), or after `wait` (`"5s"`) at the latest, keeping the last 64 KB. For answering a debugger prompt, as `carte attach` does. `process` picks the process; it defaults to the one paused at a debugger prompt, else the HTTP one. The quiet time starts at the echo of what was typed, so raise `quiet` for commands that take a while to answer |

A start or restart that fails returns an error with the last log lines.

There are no tools for sharing, `stop-all`, saving a pin or disable to the
project file, or revealing env values: an agent has to ask you, and you
run those yourself.
