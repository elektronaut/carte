//! The daemon's API over its Unix socket: JSON requests, newline-delimited JSON streams.

use std::convert::Infallible;

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use carte_api::{
    Address, ControlRequest, ErrorBody, EventsRequest, LogsRequest, MarkRequest, Problem,
    PruneRequest, ShareRequest, StopAllRequest, UnshareRequest, paths,
};
use carte_core::address;
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use crate::paths::Paths;
use crate::supervisor::Handle;

#[derive(Clone)]
pub struct Api {
    pub supervisor: Handle,
    pub paths: Paths,
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
        .route(paths::MARK, post(mark))
        .route(paths::EVENTS, post(events))
        .route(paths::RELOAD, post(reload))
        .route(paths::ENV, get(daemon_env))
        .route(paths::PROCESS_ENV, post(process_env))
        .route(paths::SHARE, post(share))
        .route(paths::UNSHARE, post(unshare))
        .route(paths::PRUNE, get(prune_list).post(prune))
        .route(paths::HOLD, post(hold))
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

/// A 404 answer when the address names nothing in the current world.
fn unresolved(api: &Api, target: &Address) -> Option<Response> {
    let parsed: address::Address = match target.address.parse() {
        Ok(parsed) => parsed,
        Err(err) => return Some(error(StatusCode::NOT_FOUND, err.to_string())),
    };
    let world = api.supervisor.world();
    let cwd = target.cwd.as_deref();
    let resolved = match &target.project {
        Some(project) => address::resolve_in_project(&world, project, &parsed, cwd),
        None => address::resolve(&world, &parsed, cwd),
    };
    resolved
        .err()
        .map(|err| error(StatusCode::NOT_FOUND, err.to_string()))
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
    if let Some(response) = unresolved(&api, &request.target) {
        return response;
    }
    match api.supervisor.control(request).await {
        Ok(response) => Json(response).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn stop_all(State(api): State<Api>, Json(request): Json<StopAllRequest>) -> Response {
    Json(api.supervisor.stop_all(request.idle_only).await).into_response()
}

async fn logs(State(api): State<Api>, Json(request): Json<LogsRequest>) -> Response {
    if let Some(response) = unresolved(&api, &request.target) {
        return response;
    }
    let mut follow =
        (request.follow && request.rank.is_none()).then(|| api.supervisor.follow_logs());
    let (lines, filter) = match api.supervisor.logs(request).await {
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

async fn mark(State(api): State<Api>, Json(request): Json<MarkRequest>) -> Response {
    if let Some(response) = unresolved(&api, &request.target) {
        return response;
    }
    match api.supervisor.mark(request.target).await {
        Ok(marker) => Json(marker).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn events(State(api): State<Api>, Json(request): Json<EventsRequest>) -> Response {
    if let Some(response) = request
        .target
        .as_ref()
        .and_then(|target| unresolved(&api, target))
    {
        return response;
    }
    let mut follow = request.follow.then(|| api.supervisor.follow_events());
    let tail = request.tail.unwrap_or(100);
    let (events, filter) = match api.supervisor.events(request.target, tail).await {
        Ok(result) => result,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let last = events.last().map_or(0, |e| e.seq);
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        for event in &events {
            if tx.send(ndjson(event)).await.is_err() {
                return;
            }
        }
        let Some(follow) = follow.as_mut() else {
            return;
        };
        loop {
            match follow.recv().await {
                Ok(event) if event.seq > last && filter.matches(&event) => {
                    if tx.send(ndjson(&event)).await.is_err() {
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
    if let Some(response) = unresolved(&api, &request.target) {
        return response;
    }
    match api.supervisor.share(request).await {
        Ok(response) => Json(response).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn unshare(State(api): State<Api>, Json(request): Json<UnshareRequest>) -> Response {
    if let Some(response) = unresolved(&api, &request.target) {
        return response;
    }
    match api.supervisor.unshare(request.target).await {
        Ok(apps) => Json(apps).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

/// Holds the instance until the client goes away, which drops the response body.
async fn hold(State(api): State<Api>, Json(target): Json<Address>) -> Response {
    if let Some(response) = unresolved(&api, &target) {
        return response;
    }
    let (key, id) = match api.supervisor.hold(target).await {
        Ok(hold) => hold,
        Err(message) => return error(StatusCode::CONFLICT, message),
    };
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        tx.closed().await;
        api.supervisor.release(key, id);
    });
    stream(rx)
}

async fn prune_list(State(api): State<Api>) -> Response {
    let running = api.supervisor.running().await;
    let paths = api.paths.clone();
    match tokio::task::spawn_blocking(move || crate::prune::list(&paths, running)).await {
        Ok(list) => Json(list).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

/// Removes the asked-for leftovers that still are leftovers once the configuration is
/// read again, and that no running instance owns when the supervisor gets to them.
async fn prune(State(api): State<Api>, Json(request): Json<PruneRequest>) -> Response {
    let running = api.supervisor.running().await;
    let paths = api.paths.clone();
    let selected =
        tokio::task::spawn_blocking(move || crate::prune::select(&paths, running, &request.paths))
            .await;
    let (selected, kept) = match selected {
        Ok(selected) => selected,
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    };
    let sizes = selected
        .iter()
        .map(|(orphan, bytes)| (orphan.path.clone(), *bytes))
        .collect();
    let orphans: Vec<_> = selected.into_iter().map(|(orphan, _)| orphan).collect();
    let count = orphans.len();
    let detached = api.supervisor.prune(orphans).await;
    if detached.len() != count {
        return error(StatusCode::SERVICE_UNAVAILABLE, "carte is shutting down");
    }
    match tokio::task::spawn_blocking(move || crate::prune::finish(detached, &sizes, kept)).await {
        Ok(result) => Json(result).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

async fn daemon_env(State(api): State<Api>) -> Response {
    Json(carte_api::DaemonEnv {
        vars: crate::supervisor::base_env(),
        run_root: Some(api.paths.run_root.clone()),
    })
    .into_response()
}

async fn process_env(State(api): State<Api>, Json(target): Json<Address>) -> Response {
    let world = api.supervisor.world();
    let snapshot = api.supervisor.snapshot();
    let paths = api.paths.clone();
    let rendered = tokio::task::spawn_blocking(move || {
        crate::process_env::for_address(&paths, &world, &snapshot, &target)
    })
    .await;
    match rendered {
        Ok(Ok(response)) => Json(response).into_response(),
        Ok(Err(err)) => error(StatusCode::BAD_REQUEST, format!("{err:#}")),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
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
