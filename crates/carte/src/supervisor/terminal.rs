//! A process's pty as attached clients share it: output as it comes, input from any
//! of them, and a size that fits the smallest attached terminal.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use carte_api::attach::TermSize;
use tokio::sync::{broadcast, watch};

use super::runner::ProcLog;
use crate::process::Pty;

pub struct Terminal {
    pty: Pty,
    log: Arc<Mutex<ProcLog>>,
    /// The size the process started with, which it gets back when the last
    /// terminal detaches.
    configured: TermSize,
    output: broadcast::Sender<Bytes>,
    clients: Mutex<Clients>,
    /// Held while one client's input is written, so another's can't split it.
    writing: tokio::sync::Mutex<()>,
    ended: watch::Sender<Option<String>>,
}

struct Clients {
    next: u64,
    sizes: BTreeMap<u64, Option<TermSize>>,
    current: TermSize,
}

impl Terminal {
    pub(super) fn new(pty: Pty, log: Arc<Mutex<ProcLog>>, size: TermSize) -> Arc<Self> {
        Arc::new(Terminal {
            pty,
            log,
            configured: size,
            output: broadcast::channel(1024).0,
            clients: Mutex::new(Clients {
                next: 0,
                sizes: BTreeMap::new(),
                current: size,
            }),
            writing: tokio::sync::Mutex::new(()),
            ended: watch::channel(None).0,
        })
    }

    /// Hands output to the attached clients. Called with the log locked, right after
    /// the output went into it, so a client that joins gets each byte once.
    pub(super) fn output(&self, data: &[u8]) {
        if self.output.receiver_count() > 0 {
            let _ = self.output.send(Bytes::copy_from_slice(data));
        }
    }

    /// Marks the process as gone; attached clients get `message` and are let go.
    pub(super) fn end(&self, message: String) {
        self.ended.send_replace(Some(message));
    }

    /// Attaches a client. `size` takes part in sizing the pty; with `replay` the
    /// client gets the last screen of output, as many lines as its terminal has rows.
    pub fn join(self: &Arc<Self>, size: Option<TermSize>, replay: bool) -> Client {
        let size = size.filter(|s| s.cols > 0 && s.rows > 0);
        let (output, replay) = {
            let log = self.log.lock().unwrap();
            let output = self.output.subscribe();
            let rows = size.unwrap_or(self.configured).rows;
            let mut replay = if replay {
                log.raw.last_lines(usize::from(rows))
            } else {
                Vec::new()
            };
            replay.drain(..replay.len().saturating_sub(1024 * 1024));
            (output, replay)
        };
        let id = {
            let mut clients = self.clients.lock().unwrap();
            clients.next += 1;
            let id = clients.next;
            clients.sizes.insert(id, size);
            self.fit(&mut clients);
            id
        };
        Client {
            terminal: self.clone(),
            id,
            output,
            replay,
            ended: self.ended.subscribe(),
        }
    }

    /// Clients attached right now.
    pub fn clients(&self) -> usize {
        self.clients.lock().unwrap().sizes.len()
    }

    /// The pty's size right now.
    pub fn size(&self) -> TermSize {
        self.clients.lock().unwrap().current
    }

    /// Writes all of `data` before anyone else's input.
    pub async fn write(&self, data: &[u8]) -> io::Result<()> {
        let _turn = self.writing.lock().await;
        self.pty.write(data).await
    }

    /// Gives the pty the smallest size among the clients that have one, or the
    /// configured size when none has.
    fn fit(&self, clients: &mut Clients) {
        let size = clients
            .sizes
            .values()
            .flatten()
            .copied()
            .reduce(|a, b| TermSize {
                cols: a.cols.min(b.cols),
                rows: a.rows.min(b.rows),
            })
            .unwrap_or(self.configured);
        if size != clients.current && self.pty.resize(size.cols, size.rows).is_ok() {
            clients.current = size;
        }
    }
}

/// One attached client; dropping it detaches.
pub struct Client {
    terminal: Arc<Terminal>,
    id: u64,
    pub output: broadcast::Receiver<Bytes>,
    /// The last screen of output from before it joined.
    pub replay: Vec<u8>,
    /// How the process exited, once it has.
    pub ended: watch::Receiver<Option<String>>,
}

impl Client {
    pub async fn write(&self, data: &[u8]) -> io::Result<()> {
        self.terminal.write(data).await
    }

    /// Writes the client's input from another task, so a process that doesn't read
    /// can't hold up the client.
    pub fn writer(&self) -> Writer {
        Writer(self.terminal.clone())
    }

    pub fn resize(&self, size: TermSize) {
        if size.cols == 0 || size.rows == 0 {
            return;
        }
        let mut clients = self.terminal.clients.lock().unwrap();
        clients.sizes.insert(self.id, Some(size));
        self.terminal.fit(&mut clients);
    }
}

pub struct Writer(Arc<Terminal>);

impl Writer {
    pub async fn write(&self, data: &[u8]) -> io::Result<()> {
        self.0.write(data).await
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let mut clients = self.terminal.clients.lock().unwrap();
        clients.sizes.remove(&self.id);
        self.terminal.fit(&mut clients);
    }
}
