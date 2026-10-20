//! `carte mcp`: a Model Context Protocol server over stdio, for agents. It answers
//! JSON-RPC requests one per line and resolves addresses from its working directory,
//! like the CLI.

use std::io::{BufRead, Write};
use std::process::ExitCode;

use carte_api::Action;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{EventsArgs, LogsArgs, Target, attach, control, env};
use crate::paths::Paths;

/// Protocol versions this server speaks, newest last.
const VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

const INSTRUCTIONS: &str = "carte runs this machine's local web apps on demand: each app is \
served at its own https URL, and the first request starts it. Don't start dev servers \
yourself. Addresses are `app@stack/process`; every part is optional and inferred from \
the directory this server runs in.";

pub fn run(paths: &Paths, control: bool) -> anyhow::Result<ExitCode> {
    let server = Server {
        paths: paths.clone(),
        control,
    };
    let stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle_line(&line) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

struct Server {
    paths: Paths,
    control: bool,
}

impl Server {
    /// The reply to one line of input; notifications get none.
    fn handle_line(&self, line: &str) -> Option<Value> {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(err) => return Some(error(Value::Null, -32700, &format!("parse error: {err}"))),
        };
        let id = message.get("id").cloned()?;
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        Some(match method {
            "initialize" => reply(id, initialize(&params)),
            "ping" => reply(id, json!({})),
            "tools/list" => reply(id, json!({ "tools": self.tools() })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let arguments = params.get("arguments").cloned().unwrap_or(json!({}));
                match self.call(name, arguments) {
                    Some(result) => reply(id, result),
                    None => error(id, -32602, &format!("unknown tool {name:?}")),
                }
            }
            _ => error(id, -32601, &format!("method {method:?} not found")),
        })
    }

    fn tools(&self) -> Vec<Value> {
        let mut tools = vec![
            tool(
                "status",
                "The state of an app (or the running apps of a stack or project): URL, \
                 state, public URL, processes, last error, what keeps it running.",
                json!({}),
                &[],
            ),
            tool(
                "logs",
                "Recent log lines, as JSON lines with app, stack, process, time and text.",
                json!({
                    "lines": { "type": "integer", "description": "How many lines (default 100)." },
                    "since": { "type": "string", "description": "Only lines from this long ago on, like `5m`." },
                    "since_start": { "type": "boolean", "description": "Only lines from each process's latest run." },
                    "search": { "type": "string", "description": "Only lines containing this text." },
                }),
                &[],
            ),
            tool(
                "logs_search",
                "The log lines most relevant to some words, best first.",
                json!({
                    "query": { "type": "string", "description": "Words to look for, like an error message." },
                    "top": { "type": "integer", "description": "How many lines (default 20)." },
                }),
                &["query"],
            ),
            tool(
                "events",
                "What carte did and why: starts, stops (idle, evicted, by hand), crashes, \
                 restarts, config reloads, worktrees appearing.",
                json!({
                    "limit": { "type": "integer", "description": "How many events (default 50)." },
                }),
                &[],
            ),
            tool(
                "env",
                "The environment a process starts with and where each value comes from. \
                 Values from the environment and env files are hidden.",
                json!({
                    "process": { "type": "string", "description": "The process; defaults to the one serving HTTP." },
                }),
                &[],
            ),
        ];
        if self.control {
            tools.extend([
                tool(
                    "start",
                    "Start an app, stack or process and wait until it's ready; failures \
                     come with the last log lines.",
                    json!({}),
                    &[],
                ),
                tool(
                    "restart",
                    "Restart an app, stack or process and wait until it's ready; failures \
                     come with the last log lines.",
                    json!({}),
                    &[],
                ),
                tool("stop", "Stop an app, stack or process.", json!({}), &[]),
                tool(
                    "type",
                    "Type into a running process's terminal, like a command at a debugger \
                     prompt (`(rdbg)`, `pry`, `irb`, `(byebug)`), and return what it printed \
                     until its output went quiet.",
                    json!({
                        "text": { "type": "string", "description": "What to type, like `continue` or `\\u0003` for ctrl-c." },
                        "enter": { "type": "boolean", "description": "Press enter after the text (default true)." },
                        "process": { "type": "string", "description": "The process; defaults to the one serving HTTP." },
                        "quiet_ms": { "type": "integer", "description": "Return once the output has been quiet this long (default 500); raise it for commands that take a while before they print." },
                        "wait": { "type": "string", "description": "Return after this long at the latest, like `30s` (default `5s`)." },
                    }),
                    &["text"],
                ),
            ]);
        }
        tools
    }

    fn call(&self, name: &str, arguments: Value) -> Option<Value> {
        let paths = &self.paths;
        let outcome = match name {
            "status" => parse::<Addressed>(arguments)
                .and_then(|args| pretty(&control::status_response(paths, &args.target())?)),
            "logs" => parse::<LogsTool>(arguments).and_then(|args| {
                let since = args
                    .since
                    .as_deref()
                    .map(carte_core::duration::parse)
                    .transpose()?;
                let request = control::logs_request(LogsArgs {
                    target: args.at.target(),
                    follow: false,
                    lines: args.lines,
                    search: args.search,
                    since,
                    since_start: args.since_start,
                    rank: None,
                    top: 0,
                    mark: false,
                });
                json_lines(&control::collect_logs(paths, &request)?)
            }),
            "logs_search" => parse::<SearchTool>(arguments).and_then(|args| {
                let request = control::logs_request(LogsArgs {
                    target: args.at.target(),
                    follow: false,
                    lines: None,
                    search: None,
                    since: None,
                    since_start: false,
                    rank: Some(args.query),
                    top: args.top.unwrap_or(20),
                    mark: false,
                });
                json_lines(&control::collect_logs(paths, &request)?)
            }),
            "events" => parse::<EventsTool>(arguments).and_then(|args| {
                let request = control::events_request(EventsArgs {
                    address: args.at.address,
                    project: args.at.project,
                    follow: false,
                    lines: args.limit.unwrap_or(50),
                });
                json_lines(&control::collect_events(paths, &request)?)
            }),
            "env" => parse::<EnvTool>(arguments).and_then(|args| {
                let (rendered, _) = env::render(paths, &args.at.target(), args.process)?;
                pretty(&rendered.response(true))
            }),
            "start" if self.control => self.control_tool(arguments, Action::Up, true),
            "restart" if self.control => self.control_tool(arguments, Action::Restart, true),
            "stop" if self.control => self.control_tool(arguments, Action::Down, false),
            "type" if self.control => parse::<TypeTool>(arguments).and_then(|args| {
                let mut input = args.text.into_bytes();
                if args.enter.unwrap_or(true) {
                    input.push(b'\r');
                }
                let wait = args
                    .wait
                    .as_deref()
                    .map(carte_core::duration::parse)
                    .transpose()?
                    .unwrap_or(std::time::Duration::from_secs(5));
                let quiet = std::time::Duration::from_millis(args.quiet_ms.unwrap_or(500));
                attach::type_into(paths, &args.at.target(), args.process, input, wait, quiet)
            }),
            _ => return None,
        };
        Some(match outcome {
            Ok(text) => result(&text, false),
            Err(err) => result(&format!("{err:#}"), true),
        })
    }

    fn control_tool(&self, arguments: Value, action: Action, wait: bool) -> anyhow::Result<String> {
        let args = parse::<Addressed>(arguments)?;
        let response = control::send_control(&self.paths, &args.target(), action, wait)?;
        let text = pretty(&response)?;
        if response.failures.is_empty() {
            Ok(text)
        } else {
            Err(anyhow::anyhow!(text))
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Addressed {
    address: Option<String>,
    project: Option<String>,
}

impl Addressed {
    fn target(&self) -> Target {
        Target {
            address: self.address.clone(),
            project: self.project.clone(),
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LogsTool {
    #[serde(flatten)]
    at: Addressed,
    lines: Option<usize>,
    since: Option<String>,
    since_start: bool,
    search: Option<String>,
}

#[derive(Deserialize)]
struct SearchTool {
    #[serde(flatten)]
    at: Addressed,
    query: String,
    top: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct EventsTool {
    #[serde(flatten)]
    at: Addressed,
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct TypeTool {
    #[serde(flatten)]
    at: Addressed,
    text: String,
    enter: Option<bool>,
    process: Option<String>,
    quiet_ms: Option<u64>,
    wait: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct EnvTool {
    #[serde(flatten)]
    at: Addressed,
    process: Option<String>,
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .filter(|v| VERSIONS.contains(v))
        .unwrap_or(VERSIONS[VERSIONS.len() - 1]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "carte", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

/// A tool taking an address and a project besides `properties`.
fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    let mut all = json!({
        "address": {
            "type": "string",
            "description": "`app@stack/process`; every part is optional and inferred from the current directory.",
        },
        "project": {
            "type": "string",
            "description": "Resolve the address inside this project; alone, the whole project.",
        },
    });
    if let (Some(all), Value::Object(more)) = (all.as_object_mut(), properties) {
        all.extend(more);
    }
    json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": all, "required": required },
    })
}

fn parse<T: DeserializeOwned>(arguments: Value) -> anyhow::Result<T> {
    serde_json::from_value(arguments).map_err(|err| anyhow::anyhow!("invalid arguments: {err}"))
}

fn pretty<T: serde::Serialize>(value: &T) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(value)?)
}

fn json_lines<T: serde::Serialize>(items: &[T]) -> anyhow::Result<String> {
    let lines: Vec<String> = items
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<_, _>>()?;
    Ok(lines.join("\n"))
}

fn result(text: &str, is_error: bool) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn reply(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(control: bool) -> (Server, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let server = Server {
            paths: Paths::under(dir.path()),
            control,
        };
        (server, dir)
    }

    fn names(server: &Server) -> Vec<String> {
        let reply = server
            .handle_line(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .unwrap();
        reply["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn control_tools_only_with_the_flag() {
        assert_eq!(
            names(&server(false).0),
            ["status", "logs", "logs_search", "events", "env"]
        );
        assert_eq!(
            names(&server(true).0),
            [
                "status",
                "logs",
                "logs_search",
                "events",
                "env",
                "start",
                "restart",
                "stop",
                "type"
            ]
        );
    }

    #[test]
    fn control_tools_are_refused_without_the_flag() {
        let reply = server(false)
            .0
            .handle_line(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"stop","arguments":{}}}"#,
            )
            .unwrap();
        assert_eq!(reply["error"]["code"], -32602);
    }

    #[test]
    fn answers_the_protocol_version_it_speaks() {
        let (server, _dir) = server(false);
        let ask = |version: &str| {
            let line = format!(
                r#"{{"jsonrpc":"2.0","id":0,"method":"initialize","params":{{"protocolVersion":"{version}"}}}}"#
            );
            server.handle_line(&line).unwrap()["result"]["protocolVersion"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(ask("2025-03-26"), "2025-03-26");
        assert_eq!(ask("1999-01-01"), "2025-11-25");
    }

    #[test]
    fn ignores_notifications_and_rejects_unknown_methods() {
        let (server, _dir) = server(false);
        assert!(
            server
                .handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .is_none()
        );
        let reply = server
            .handle_line(r#"{"jsonrpc":"2.0","id":3,"method":"resources/list"}"#)
            .unwrap();
        assert_eq!(reply["error"]["code"], -32601);
        let reply = server.handle_line("{nope").unwrap();
        assert_eq!(reply["error"]["code"], -32700);
    }

    #[test]
    fn a_missing_daemon_is_a_tool_error() {
        let reply = server(false)
            .0
            .handle_line(
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"events","arguments":{}}}"#,
            )
            .unwrap();
        assert_eq!(reply["result"]["isError"], true);
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("daemon isn't running"), "{text}");
    }
}
