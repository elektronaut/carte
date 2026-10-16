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
#[cfg(test)]
mod fixtures;
mod input;
mod logs;
mod target;
mod tasks;
mod tree;
mod view;

pub fn run(paths: &Paths) -> anyhow::Result<ExitCode> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let socket = paths.socket();
    runtime.block_on(async move {
        let mut terminal = ratatui::try_init()?;
        let restore = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            tasks::set_mouse_capture(false);
            restore(info);
        }));
        tasks::set_mouse_capture(true);
        let result = event_loop(&mut terminal, socket).await;
        tasks::set_mouse_capture(false);
        ratatui::restore();
        result
    })?;
    Ok(ExitCode::SUCCESS)
}

async fn event_loop(
    terminal: &mut DefaultTerminal,
    socket: std::path::PathBuf,
) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut tasks = tasks::Tasks::start(socket, tx);
    let mut app = app::App::new(Instant::now());
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
    Ok(())
}
