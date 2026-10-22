//! `carte tui`: watch and control the daemon.
//!
//! `app` holds the state and turns events into commands, `view` draws it, `tasks`
//! does the IO, and this module runs the loop between them.

use std::process::ExitCode;
use std::time::Instant;

use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use crate::paths::Paths;

mod app;
mod browse;
#[cfg(test)]
pub(crate) mod fixtures;
mod input;
mod jump;
mod logs;
mod saved;
mod tabs;
mod target;
mod tasks;
mod theme;
mod tree;
mod view;

pub(crate) use view::format_bytes;

pub fn run(paths: &Paths) -> anyhow::Result<ExitCode> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let socket = paths.socket();
    let start_at = here(paths);
    let saved_path = saved::path(&paths.state_dir);
    let restored = saved::load(&saved_path);
    let detach_keys = carte_core::config::load_machine_config(&paths.machine_config())
        .map(|machine| machine.attach.detach_keys)
        .unwrap_or_default();
    runtime
        .block_on(async move {
            let mut terminal = ratatui::try_init()?;
            let restore = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                tasks::set_mouse_capture(false);
                restore(info);
            }));
            tasks::set_mouse_capture(true);
            let result = event_loop(&mut terminal, socket, start_at, restored, detach_keys).await;
            tasks::set_mouse_capture(false);
            ratatui::restore();
            result
        })
        .map(|saved| {
            if let Err(err) = saved::store(&saved_path, &saved) {
                eprintln!("carte: couldn't save the TUI's state: {err}");
            }
        })?;
    Ok(ExitCode::SUCCESS)
}

/// The instance whose checkout holds the current directory.
fn here(paths: &Paths) -> Option<tree::NodeId> {
    let cwd = std::env::current_dir().ok()?.canonicalize().ok()?;
    let world = carte_core::discovery::load_world(&paths.config_dir);
    let location = carte_core::address::locate(&world, &cwd)?;
    let stack = location.stack.slug().to_string();
    Some(match location.app {
        Some(app) => tree::NodeId::Instance {
            project: location.project,
            stack,
            app,
        },
        None => tree::NodeId::Stack {
            project: location.project,
            stack,
        },
    })
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    socket: std::path::PathBuf,
    start_at: Option<tree::NodeId>,
    restored: saved::Saved,
    detach_keys: carte_core::detach::DetachKeys,
) -> anyhow::Result<saved::Saved> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut tasks = tasks::Tasks::start(socket, tx);
    let mut app = app::App::new(Instant::now());
    app.start_at = start_at;
    app.detach_keys = detach_keys;
    restored.apply(&mut app.fold);
    terminal.draw(|frame| view::render(frame, &mut app))?;
    while let Some(event) = rx.recv().await {
        let mut next = Some(event);
        // Drain whatever else is queued so a burst of log lines costs one redraw.
        while let Some(event) = next {
            for command in app.update(event, Instant::now()) {
                tasks.run(command);
            }
            next = rx.try_recv().ok();
        }
        if app.quit {
            break;
        }
        terminal.draw(|frame| view::render(frame, &mut app))?;
    }
    Ok(saved::Saved::from_fold(&app.fold, &app.roots))
}
