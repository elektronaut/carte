//! The daemon's side of `carte attach`: an upgraded connection that carries a
//! process's output to the client and the client's typing to the process.

use std::io;

use bytes::Bytes;
use carte_api::attach::{Attached, Frame, FrameReader};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::broadcast::error::RecvError;

use crate::supervisor::terminal::Client;

enum Step {
    Read(io::Result<usize>),
    Output(Result<Bytes, RecvError>),
    Ended(String),
}

/// Runs one attached client until either side goes away or the process exits.
pub async fn session<S: AsyncRead + AsyncWrite + Unpin>(
    io: S,
    attached: Attached,
    mut client: Client,
) {
    let (mut reader, mut writer) = tokio::io::split(io);
    let mut first = Frame::Attached(attached).encode();
    if !client.replay.is_empty() {
        first.extend(Frame::Output(std::mem::take(&mut client.replay)).encode());
    }
    if writer.write_all(&first).await.is_err() {
        return;
    }
    let mut frames = FrameReader::default();
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let step = tokio::select! {
            read = reader.read(&mut buf) => Step::Read(read),
            data = client.output.recv() => Step::Output(data),
            ended = client.ended.wait_for(Option::is_some) => match ended {
                Ok(message) => Step::Ended(message.clone().unwrap_or_default()),
                Err(_) => return,
            },
        };
        match step {
            Step::Read(Ok(0) | Err(_)) => return,
            Step::Read(Ok(n)) => {
                frames.feed(&buf[..n]);
                loop {
                    match frames.next_frame() {
                        Ok(Some(Frame::Input(data))) => {
                            if client.write(&data).await.is_err() {
                                return;
                            }
                        }
                        Ok(Some(Frame::Resize(size))) => client.resize(size),
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(_) => return,
                    }
                }
            }
            Step::Output(Ok(data)) => {
                if writer
                    .write_all(&Frame::Output(data.to_vec()).encode())
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Step::Output(Err(RecvError::Lagged(_))) => {}
            Step::Output(Err(RecvError::Closed)) => return,
            Step::Ended(message) => {
                let mut last = Vec::new();
                while let Ok(data) = client.output.try_recv() {
                    last.extend(Frame::Output(data.to_vec()).encode());
                }
                last.extend(Frame::Ended(message).encode());
                let _ = writer.write_all(&last).await;
                let _ = writer.shutdown().await;
                return;
            }
        }
    }
}
