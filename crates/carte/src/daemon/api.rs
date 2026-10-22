//! The daemon's API over its Unix socket: JSON requests, newline-delimited JSON streams.

use std::convert::Infallible;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, State};
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use carte_api::attach::{self, AttachRequest};
use carte_api::{
    Address, ControlRequest, ErrorBody, EventSubject, EventsRequest, LogsRequest, MarkRequest,
    Problem, PruneRequest, RequestsRequest, ShareRequest, StopAllRequest, UnshareRequest, paths,
};
use carte_api::{Event, KubeServicesRequest, KubeServicesResponse, Snapshot};
use carte_core::address;
use carte_core::kube::{self, KubeAddress};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;

use hyper_util::rt::TokioIo;

use super::requests::{Entry, RequestLog};
use crate::kube::Kube;
use crate::paths::Paths;
use crate::resolved_config::ConfigError;
use crate::supervisor::{AttachError, EventFilter, Handle};

#[derive(Clone)]
pub struct Api {
    pub supervisor: Handle,
    pub kube: Kube,
    pub paths: Paths,
    pub requests: Arc<RequestLog>,
    /// What the supervisor does with every process.
    pub limits: crate::resolved_config::ProcessLimits,
    /// Reloads the configuration from disk and returns its problems.
    pub reload: Arc<dyn Fn() -> Vec<Problem> + Send + Sync>,
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
        .route(paths::REQUESTS, post(requests))
        .route(paths::RELOAD, post(reload))
        .route(paths::ENV, get(daemon_env))
        .route(paths::PROCESS_ENV, post(process_env))
        .route(paths::CONFIG, post(config))
        .route(paths::SHARE, post(share))
        .route(paths::UNSHARE, post(unshare))
        .route(paths::PRUNE, get(prune_list).post(prune))
        .route(paths::HOLD, post(hold))
        .route(paths::KUBE_SERVICES, post(kube_services))
        .route(paths::ATTACH, post(attach))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::map_response(with_version))
        .with_state(api)
}

/// Tells clients which carte answers, so they can tell an outdated daemon apart.
async fn with_version(mut response: Response) -> Response {
    response.headers_mut().insert(
        HeaderName::from_static(carte_api::VERSION_HEADER),
        HeaderValue::from_static(env!("CARGO_PKG_VERSION")),
    );
    response
}

/// A JSON request body; one that can't be read is answered with an `ErrorBody`
/// saying why, as every other error is.
struct JsonBody<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for JsonBody<T> {
    type Rejection = Response;

    async fn from_request(request: axum::extract::Request, state: &S) -> Result<Self, Response> {
        match Json::<T>::from_request(request, state).await {
            Ok(Json(value)) => Ok(JsonBody(value)),
            Err(rejection) => Err(error(rejection.status(), rejection.body_text())),
        }
    }
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

/// What the address names in a kube project, or `None` when it isn't about one.
fn kube_address(api: &Api, target: &Address) -> Option<Result<KubeAddress, String>> {
    let parsed: address::Address = match target.address.parse() {
        Ok(parsed) => parsed,
        Err(err) => return Some(Err(err.to_string())),
    };
    let world = api.supervisor.world();
    kube::resolve_address(&world, target.project.as_deref(), &parsed)
        .map(|resolved| resolved.map_err(|err| err.to_string()))
}

/// A 400 answer for an address in a kube project, which `what` doesn't apply to.
fn not_for_kube(api: &Api, target: &Address, what: &str) -> Option<Response> {
    kube_address(api, target).map(|_| {
        error(
            StatusCode::BAD_REQUEST,
            format!("kube projects' Services have no {what}"),
        )
    })
}

/// The supervisor's snapshot with the kube projects added.
fn full_snapshot(api: &Api) -> Snapshot {
    let mut snapshot = api.supervisor.snapshot().as_ref().clone();
    let world = api.supervisor.world();
    snapshot.projects.extend(api.kube.status(&world));
    snapshot.running_instances += api.kube.running();
    snapshot
}

/// Whether an event is about what a kube address names.
fn kube_event_matches(address: &KubeAddress, event: &Event) -> bool {
    event
        .subjects
        .iter()
        .any(|subject| kube_subject_matches(address, subject))
}

fn kube_subject_matches(address: &KubeAddress, subject: &EventSubject) -> bool {
    subject.project == address.project
        && address
            .namespace
            .as_ref()
            .is_none_or(|n| subject.stack.as_ref() == Some(n))
        && address
            .service
            .as_ref()
            .is_none_or(|s| subject.app.as_ref() == Some(s))
}

/// A 404 answer when the address names nothing in the current world.
fn unresolved(api: &Api, target: &Address) -> Option<Response> {
    resolve(api, target)
        .err()
        .map(|message| error(StatusCode::NOT_FOUND, message))
}

/// What the address names in the current world, or why it names nothing.
fn resolve(api: &Api, target: &Address) -> Result<address::Target, String> {
    let parsed: address::Address = target
        .address
        .parse()
        .map_err(|err: address::AddressError| err.to_string())?;
    let world = api.supervisor.world();
    let cwd = target.cwd.as_deref();
    let resolved = match &target.project {
        Some(project) => address::resolve_in_project(&world, project, &parsed, cwd),
        None => address::resolve(&world, &parsed, cwd),
    };
    resolved.map_err(|err| err.to_string())
}

async fn snapshot(State(api): State<Api>) -> Response {
    Json(full_snapshot(&api)).into_response()
}

async fn watch(State(api): State<Api>) -> Response {
    let mut watch = api.supervisor.watch();
    let mut kube = api.kube.changes();
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(async move {
        loop {
            watch.borrow_and_update();
            kube.borrow_and_update();
            if tx.send(ndjson(&full_snapshot(&api))).await.is_err() {
                break;
            }
            let changed = tokio::select! {
                _ = tx.closed() => false,
                changed = watch.changed() => changed.is_ok(),
                changed = kube.changed() => changed.is_ok(),
            };
            if !changed {
                break;
            }
        }
    });
    stream(rx)
}

async fn control(State(api): State<Api>, JsonBody(request): JsonBody<ControlRequest>) -> Response {
    if let Some(address) = kube_address(&api, &request.target) {
        let result = match address {
            Ok(address) => api.kube.control(address, request.action).await,
            Err(message) => Err(message),
        };
        return match result {
            Ok(response) => Json(response).into_response(),
            Err(message) => error(StatusCode::BAD_REQUEST, message),
        };
    }
    if let Some(response) = unresolved(&api, &request.target) {
        return response;
    }
    match api.supervisor.control(request).await {
        Ok(response) => Json(response).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn stop_all(State(api): State<Api>, JsonBody(request): JsonBody<StopAllRequest>) -> Response {
    let mut response = api.supervisor.stop_all(request.idle_only).await;
    response
        .affected
        .extend(api.kube.stop_all(request.idle_only));
    Json(response).into_response()
}

async fn logs(State(api): State<Api>, JsonBody(request): JsonBody<LogsRequest>) -> Response {
    if let Some(address) = kube_address(&api, &request.target) {
        let lines = match address {
            Ok(address) => api.kube.logs(address, &request).await,
            Err(message) => Err(message),
        };
        let mut lines = match lines {
            Ok(lines) => lines,
            Err(message) => return error(StatusCode::BAD_REQUEST, message),
        };
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            loop {
                let line = tokio::select! {
                    _ = tx.closed() => return,
                    line = lines.recv() => line,
                };
                let Some(line) = line else {
                    return;
                };
                if tx.send(ndjson(&line)).await.is_err() {
                    return;
                }
            }
        });
        return stream(rx);
    }
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
        forward(&tx, follow, |line| filter.matches(line)).await;
    });
    stream(rx)
}

async fn mark(State(api): State<Api>, JsonBody(request): JsonBody<MarkRequest>) -> Response {
    if let Some(response) = not_for_kube(&api, &request.target, "log markers") {
        return response;
    }
    if let Some(response) = unresolved(&api, &request.target) {
        return response;
    }
    match api.supervisor.mark(request.target).await {
        Ok(marker) => Json(marker).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn events(State(api): State<Api>, JsonBody(request): JsonBody<EventsRequest>) -> Response {
    if let Some(address) = request
        .target
        .as_ref()
        .and_then(|target| kube_address(&api, target))
    {
        return match address {
            Ok(address) => kube_events(api, address, request.follow, request.tail).await,
            Err(message) => error(StatusCode::NOT_FOUND, message),
        };
    }
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
        forward(&tx, follow, |event| {
            event.seq > last && filter.matches(event)
        })
        .await;
    });
    stream(rx)
}

/// The events about a kube address, which the supervisor can't resolve itself.
async fn kube_events(
    api: Api,
    address: KubeAddress,
    follow: bool,
    tail: Option<usize>,
) -> Response {
    let mut follow = follow.then(|| api.supervisor.follow_events());
    let events = match api.supervisor.events(None, usize::MAX).await {
        Ok((events, _)) => events,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let mut events: Vec<Event> = events
        .into_iter()
        .filter(|e| kube_event_matches(&address, e))
        .collect();
    let skip = events.len().saturating_sub(tail.unwrap_or(100));
    events.drain(..skip);
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
        forward(&tx, follow, |event| {
            event.seq > last && kube_event_matches(&address, event)
        })
        .await;
    });
    stream(rx)
}

/// Which requests a client wants, by the address it gave.
enum RequestScope {
    All,
    Kube(KubeAddress),
    Instances(EventFilter),
}

impl RequestScope {
    fn matches(&self, entry: &Entry) -> bool {
        let subject = entry.handled.resolution.subject.as_ref();
        match self {
            RequestScope::All => true,
            RequestScope::Kube(address) => {
                subject.is_some_and(|subject| kube_subject_matches(address, subject))
            }
            RequestScope::Instances(filter) => {
                subject.is_some_and(|subject| filter.matches_subject(subject))
            }
        }
    }
}

async fn requests(
    State(api): State<Api>,
    JsonBody(request): JsonBody<RequestsRequest>,
) -> Response {
    let scope = match &request.target {
        None => RequestScope::All,
        Some(target) => match kube_address(&api, target) {
            Some(Ok(address)) => RequestScope::Kube(address),
            Some(Err(message)) => return error(StatusCode::NOT_FOUND, message),
            None => match resolve(&api, target) {
                Ok(target) => RequestScope::Instances(EventFilter::for_target(target)),
                Err(message) => return error(StatusCode::NOT_FOUND, message),
            },
        },
    };
    let mut follow = request.follow.then(|| api.requests.follow());
    let entries = api
        .requests
        .recent(|entry| scope.matches(entry), request.tail.unwrap_or(100));
    let last = entries.last().map_or(0, |e| e.seq);
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        for entry in &entries {
            if tx.send(ndjson(&entry.to_api())).await.is_err() {
                return;
            }
        }
        let Some(follow) = follow.as_mut() else {
            return;
        };
        forward_with(&tx, follow, |entry: &Arc<Entry>| {
            (entry.seq > last && scope.matches(entry)).then(|| ndjson(&entry.to_api()))
        })
        .await;
    });
    stream(rx)
}

async fn share(State(api): State<Api>, JsonBody(request): JsonBody<ShareRequest>) -> Response {
    if kube_address(&api, &request.target).is_some() {
        return error(
            StatusCode::BAD_REQUEST,
            "kube projects can't be shared: their Services are reachable only from this machine",
        );
    }
    if let Some(response) = unresolved(&api, &request.target) {
        return response;
    }
    match api.supervisor.share(request).await {
        Ok(response) => Json(response).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

async fn unshare(State(api): State<Api>, JsonBody(request): JsonBody<UnshareRequest>) -> Response {
    if let Some(response) = not_for_kube(&api, &request.target, "shares") {
        return response;
    }
    if let Some(response) = unresolved(&api, &request.target) {
        return response;
    }
    match api.supervisor.unshare(request.target).await {
        Ok(apps) => Json(apps).into_response(),
        Err(message) => error(StatusCode::BAD_REQUEST, message),
    }
}

/// Holds the instance until the client goes away, which drops the response body.
async fn hold(State(api): State<Api>, JsonBody(target): JsonBody<Address>) -> Response {
    if let Some(response) = not_for_kube(&api, &target, "processes to run commands beside") {
        return response;
    }
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

/// Connects the client to a process's pty: answers `101 Switching Protocols` and
/// carries attach frames over the connection from then on.
async fn attach(State(api): State<Api>, mut request: axum::extract::Request) -> Response {
    let upgrade = request
        .extensions_mut()
        .remove::<hyper::upgrade::OnUpgrade>();
    let request_upgrade = request
        .headers()
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = match axum::body::to_bytes(request.into_body(), 1024 * 1024).await {
        Ok(body) => body,
        Err(err) => return error(StatusCode::BAD_REQUEST, err.to_string()),
    };
    let request: AttachRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => return error(StatusCode::BAD_REQUEST, err.to_string()),
    };
    if let Some(response) = not_for_kube(&api, &request.target, "processes to attach to") {
        return response;
    }
    if let Some(response) = unresolved(&api, &request.target) {
        return response;
    }
    let upgrade = upgrade.filter(|_| {
        request_upgrade.is_some_and(|value| value.eq_ignore_ascii_case(attach::PROTOCOL))
    });
    let Some(upgrade) = upgrade else {
        return error(
            StatusCode::BAD_REQUEST,
            format!("attach needs `Upgrade: {}`", attach::PROTOCOL),
        );
    };
    let (attached, terminal) = match api.supervisor.attach(request.target, request.process).await {
        Ok(found) => found,
        Err(AttachError::NoProcess(message)) => return error(StatusCode::NOT_FOUND, message),
        Err(AttachError::Refused(message)) => return error(StatusCode::CONFLICT, message),
    };
    let client = terminal.join(request.size, request.replay);
    tokio::spawn(async move {
        if let Ok(upgraded) = upgrade.await {
            super::attach::session(TokioIo::new(upgraded), attached, client).await;
        }
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, attach::PROTOCOL)
        .body(Body::empty())
        .expect("valid response")
}

async fn kube_services(
    State(api): State<Api>,
    Json(request): Json<KubeServicesRequest>,
) -> Response {
    if api
        .supervisor
        .world()
        .kube_project(&request.project)
        .is_none()
    {
        return error(
            StatusCode::NOT_FOUND,
            format!("no kube project {}", request.project),
        );
    }
    match api.kube.services(&request.project).await {
        Ok(services) => Json(KubeServicesResponse { services }).into_response(),
        Err(message) => error(StatusCode::BAD_GATEWAY, message),
    }
}

async fn prune_list(State(api): State<Api>) -> Response {
    let in_use = api.supervisor.in_use().await;
    let paths = api.paths.clone();
    match tokio::task::spawn_blocking(move || crate::prune::list(&paths, in_use)).await {
        Ok(list) => Json(list).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()),
    }
}

/// Removes the asked-for leftovers that still are leftovers once the configuration is
/// read again, and that no running instance owns when the supervisor gets to them.
async fn prune(State(api): State<Api>, JsonBody(request): JsonBody<PruneRequest>) -> Response {
    let in_use = api.supervisor.in_use().await;
    let paths = api.paths.clone();
    let selected =
        tokio::task::spawn_blocking(move || crate::prune::select(&paths, in_use, &request.paths))
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

async fn process_env(State(api): State<Api>, JsonBody(target): JsonBody<Address>) -> Response {
    if let Some(response) = not_for_kube(&api, &target, "env") {
        return response;
    }
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

async fn config(State(api): State<Api>, JsonBody(target): JsonBody<Address>) -> Response {
    let world = api.supervisor.world();
    let snapshot = api.supervisor.snapshot();
    let limits = api.limits;
    let resolved = tokio::task::spawn_blocking(move || {
        crate::resolved_config::for_address(&world, &snapshot, limits, &target)
    })
    .await;
    match resolved {
        Ok(Ok(config)) => Json(config).into_response(),
        Ok(Err(ConfigError::NotFound(message))) => error(StatusCode::NOT_FOUND, message),
        Ok(Err(ConfigError::Invalid(message))) => error(StatusCode::BAD_REQUEST, message),
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

/// Sends what `follow` brings that `keep` lets through, until the client goes away or
/// nothing more can come.
async fn forward<T: Clone + Serialize>(
    tx: &mpsc::Sender<Bytes>,
    follow: &mut broadcast::Receiver<T>,
    keep: impl Fn(&T) -> bool,
) {
    forward_with(tx, follow, |item| keep(item).then(|| ndjson(item))).await;
}

/// Sends the lines `line` makes of what `follow` brings, until the client goes away
/// or nothing more can come.
async fn forward_with<T: Clone>(
    tx: &mpsc::Sender<Bytes>,
    follow: &mut broadcast::Receiver<T>,
    line: impl Fn(&T) -> Option<Bytes>,
) {
    loop {
        let item = tokio::select! {
            _ = tx.closed() => return,
            item = follow.recv() => item,
        };
        match item {
            Ok(item) => {
                if let Some(line) = line(&item)
                    && tx.send(line).await.is_err()
                {
                    return;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[tokio::test]
    async fn a_quiet_follow_ends_when_its_client_goes_away() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("test.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (lines, _) = broadcast::channel::<String>(16);
        let sender = lines.clone();
        let app = Router::new().route(
            "/follow",
            post(move || {
                let mut follow = sender.subscribe();
                async move {
                    let (tx, rx) = mpsc::channel(4);
                    tokio::spawn(async move {
                        forward(&tx, &mut follow, |line: &String| line == "wanted").await;
                    });
                    stream(rx)
                }
            }),
        );
        tokio::spawn(async move { axum::serve(listener, app).await });

        let client = crate::client::Client::new(&socket);
        let followed = tokio::time::timeout(
            Duration::from_millis(300),
            client.stream(
                axum::http::Method::POST,
                "/follow",
                Some(&()),
                |_: serde_json::Value| {},
            ),
        )
        .await;
        assert!(followed.is_err(), "the follow ended on its own");
        assert_eq!(lines.receiver_count(), 1);
        lines.send("unwanted".into()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while lines.receiver_count() > 0 {
            assert!(Instant::now() < deadline, "the follow outlived its client");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
