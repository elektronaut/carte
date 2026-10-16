//! The daemon's API over its Unix socket: JSON requests, newline-delimited JSON streams.

use std::convert::Infallible;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use carte_api::{
    ControlRequest, ErrorBody, LogsRequest, Problem, ShareRequest, StopAllRequest, UnshareRequest,
    paths,
};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use crate::supervisor::Handle;

#[derive(Clone)]
pub struct Api {
    pub supervisor: Handle,
    /// Reloads the configuration from disk and returns its problems.
    pub reload: std::sync::Arc<dyn Fn() -> Vec<Problem> + Send + Sync>,
}

pub fn router(api: Api) -> Router {
    Router::new()
        .route(paths::SNAPSHOT, get(snapshot))
        .route(paths::WATCH, get(watch))
        .route(paths::CONTROL, post(control))
        .route(paths::STOP_ALL, post(stop_all))
        .route(paths::LOGS, post(logs))
        .route(paths::RELOAD, post(reload))
        .route(paths::ENV, get(daemon_env))
        .route(paths::SHARE, post(share))
        .route(paths::UNSHARE, post(unshare))
        .with_state(api)
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorBody {
            error: message.into(),
        }),
    )
        .into_response()
}

async fn snapshot(State(api): State<Api>) -> Response {
    Json(api.supervisor.snapshot().as_ref().clone()).into_response()
}

async fn watch(State(api): State<Api>) -> Response {
    let mut watch = api.supervisor.watch();
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(async move {
        loop {
            let snapshot = watch.borrow_and_update().clone();
            if tx.send(ndjson(snapshot.as_ref())).await.is_err() || watch.changed().await.is_err() {
                break;
            }
        }
    });
    stream(rx)
}

async fn control(State(api): State<Api>, Json(request): Json<ControlRequest>) -> Response {
    match api.supervisor.control(request).await {
        Ok(response) => Json(response).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn stop_all(State(api): State<Api>, Json(request): Json<StopAllRequest>) -> Response {
    Json(api.supervisor.stop_all(request.idle_only).await).into_response()
}

async fn logs(State(api): State<Api>, Json(request): Json<LogsRequest>) -> Response {
    let mut follow = request.follow.then(|| api.supervisor.follow_logs());
    let tail = request.tail.unwrap_or(100);
    let (lines, filter) = match api
        .supervisor
        .logs(request.target, tail, request.search)
        .await
    {
        Ok(result) => result,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        for line in &lines {
            if tx.send(ndjson(line)).await.is_err() {
                return;
            }
        }
        let Some(follow) = follow.as_mut() else {
            return;
        };
        loop {
            match follow.recv().await {
                Ok(line) if filter.matches(&line) => {
                    if tx.send(ndjson(&line)).await.is_err() {
                        return;
                    }
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    stream(rx)
}

async fn share(State(api): State<Api>, Json(request): Json<ShareRequest>) -> Response {
    match api.supervisor.share(request).await {
        Ok(response) => Json(response).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn unshare(State(api): State<Api>, Json(request): Json<UnshareRequest>) -> Response {
    match api.supervisor.unshare(request.target).await {
        Ok(apps) => Json(apps).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn daemon_env() -> Response {
    Json(carte_api::DaemonEnv {
        vars: crate::supervisor::base_env(),
    })
    .into_response()
}

async fn reload(State(api): State<Api>) -> Response {
    let reload = api.reload.clone();
    match tokio::task::spawn_blocking(move || reload()).await {
        Ok(problems) => Json(problems).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

fn ndjson(value: &impl Serialize) -> Bytes {
    let mut line = serde_json::to_vec(value).unwrap_or_default();
    line.push(b'\n');
    Bytes::from(line)
}

fn stream(rx: mpsc::Receiver<Bytes>) -> Response {
    let stream = tokio_stream::StreamExt::map(ReceiverStream::new(rx), Ok::<_, Infallible>);
    Response::builder()
        .header("content-type", "application/x-ndjson")
        .body(Body::from_stream(stream))
        .expect("valid response")
}
