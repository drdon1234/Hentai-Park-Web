mod library;
use axum::{
    extract::{DefaultBodyLimit, Request as HttpRequest, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, KeepAlive},
        IntoResponse, Response, Sse,
    },
    routing::{get, post},
    Json, Router,
};
use futures_core::Stream;
use park_storage::progress::{
    Changes, ChapterProgress, Error as StorageError, Ledger, MediaPosition, PageTurn,
    ProgressSnapshot,
};
use sha2::{Digest, Sha256};
use std::{convert::Infallible, fs::OpenOptions, path::PathBuf, sync::Arc};
use subtle::ConstantTimeEq;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
struct App {
    db: Arc<Ledger>,
    downloads: PathBuf,
    wake: watch::Sender<()>,
    credential_hash: [u8; 32],
}

struct ApiError(StatusCode, &'static str);
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({"error":{"code":self.1}}))).into_response()
    }
}
impl From<StorageError> for ApiError {
    fn from(error: StorageError) -> Self {
        match error {
            StorageError::Conflict => Self(StatusCode::CONFLICT, "CONFLICT"),
            StorageError::InvalidProgress => Self(StatusCode::BAD_REQUEST, "INVALID_PROGRESS"),
            StorageError::MissingWork => Self(StatusCode::NOT_FOUND, "NOT_FOUND"),
            StorageError::Cancelled => Self(StatusCode::REQUEST_TIMEOUT, "CANCELLED"),
            _ => Self(StatusCode::SERVICE_UNAVAILABLE, "STORAGE"),
        }
    }
}

async fn authenticate(State(app): State<App>, request: HttpRequest, next: Next) -> Response {
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|v| {
            let hash: [u8; 32] = Sha256::digest(v.as_bytes()).into();
            bool::from(hash.ct_eq(&app.credential_hash))
        });
    let mut response = if authorized {
        next.run(request).await
    } else {
        ApiError(StatusCode::UNAUTHORIZED, "UNAUTHORIZED").into_response()
    };
    // 提取器默认将 JSON 错值、路径参数原文放进纯文本响应。业务错误已使用固定 JSON 契约。
    if response.status().is_client_error()
        && response.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/plain"))
    {
        *response.body_mut() = axum::body::Body::from(r#"{"error":{"code":"BAD_REQUEST"}}"#);
        response.headers_mut().remove(header::CONTENT_LENGTH);
        response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn database<T: Send + 'static>(
    job: impl FnOnce(CancellationToken) -> Result<T, StorageError> + Send + 'static,
) -> Result<T, ApiError> {
    let cancel = CancelOnDrop(CancellationToken::new());
    let request = cancel.0.clone();
    tokio::task::spawn_blocking(move || job(request))
        .await
        .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "STORAGE"))?
        .map_err(Into::into)
}

async fn snapshot(State(app): State<App>) -> Result<Json<ProgressSnapshot>, ApiError> {
    Ok(Json(
        database(move |request| app.db.snapshot(&|| request.is_cancelled())).await?,
    ))
}

async fn turn_page(
    State(app): State<App>,
    Json(turn): Json<PageTurn>,
) -> Result<Json<ChapterProgress>, ApiError> {
    let result = database(move |request| {
        let row = app.db.turn_page(turn, &|| request.is_cancelled())?;
        app.wake.send_replace(());
        Ok(row)
    })
    .await?;
    Ok(Json(result))
}

async fn position(
    State(app): State<App>,
    Json(value): Json<MediaPosition>,
) -> Result<Json<ChapterProgress>, ApiError> {
    let result = database(move |request| {
        let row = app.db.save_position(value, &|| request.is_cancelled())?;
        app.wake.send_replace(());
        Ok(row)
    })
    .await?;
    Ok(Json(result))
}

async fn history(
    State(app): State<App>,
    Json(input): Json<park_storage::web::history::HistoryWrite>,
) -> Response {
    use park_storage::web::history::Error;
    let cancel = CancelOnDrop(CancellationToken::new());
    let request = cancel.0.clone();
    let storage = app.db.clone();
    let result =
        tokio::task::spawn_blocking(move || input.apply(&storage, || request.is_cancelled())).await;
    let (status, code, version) = match result {
        Ok(Ok(value)) => {
            app.wake.send_replace(());
            return Json(value).into_response();
        }
        Ok(Err(Error::Missing)) => (StatusCode::NOT_FOUND, "NOT_FOUND", None),
        Ok(Err(Error::VersionRequired)) => {
            (StatusCode::BAD_REQUEST, "PROGRESS_VERSION_REQUIRED", None)
        }
        Ok(Err(Error::Invalid)) => (StatusCode::BAD_REQUEST, "BAD_REQUEST", None),
        Ok(Err(Error::Conflict(version))) => (StatusCode::CONFLICT, "CONFLICT", version),
        Ok(Err(Error::Ledger(StorageError::Cancelled))) => {
            (StatusCode::REQUEST_TIMEOUT, "CANCELLED", None)
        }
        _ => (StatusCode::SERVICE_UNAVAILABLE, "STORAGE", None),
    };
    let mut error = serde_json::json!({"code":code, "messageKey":format!("error.{}",code.to_lowercase()),"retryable":status.is_server_error()});
    if let Some(version) = version {
        error["details"] = serde_json::json!({"version":version});
    }
    (status, Json(serde_json::json!({"error":error}))).into_response()
}

async fn events(
    State(app): State<App>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let mut after = match headers.get("last-event-id") {
        None => 0,
        Some(value) => value
            .to_str()
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|v| *v >= 0)
            .ok_or(ApiError(StatusCode::BAD_REQUEST, "INVALID_CURSOR"))?,
    };
    let mut wake = app.wake.subscribe();
    let stream = async_stream::stream! {
        loop {
            wake.borrow_and_update();
            let db = app.db.clone();
            match database(move |_| db.changes_since(after)).await {
                Ok(Changes::Events { revision, items }) => {
                    for item in items {
                        let event = Event::default().id(item.revision.to_string()).event("progress.committed");
                        let Ok(event) = event.json_data(item) else { return };
                        yield Ok(event);
                    }
                    after = revision;
                }
                Ok(Changes::SnapshotRequired { .. }) => {
                    let db = app.db.clone();
                    let Ok(snapshot) = database(move |request| db.snapshot(&||request.is_cancelled())).await else { return };
                    after = snapshot.revision;
                    let event = Event::default().id(after.to_string()).event("progress.snapshot");
                    let Ok(event) = event.json_data(snapshot) else { return };
                    yield Ok(event);
                }
                Err(_) => return,
            }
            tokio::select! { r=wake.changed()=>if r.is_err(){return}, _=tokio::time::sleep(std::time::Duration::from_secs(2))=>{} }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn serve() -> Result<(), &'static str> {
    let credential = std::env::var("AUTH_TOKEN").map_err(|_| "missing_credential")?;
    if credential.trim().is_empty() {
        return Err("missing_credential");
    }
    let support = std::env::var_os("DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("data"));
    let file = std::env::var_os("WEB_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| support.join("web.sqlite"));
    let downloads = std::env::var_os("DOWNLOAD_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| support.join("downloads"));
    let port = std::env::var("PORT")
        .unwrap_or_else(|_| "8787".into())
        .parse::<u16>()
        .map_err(|_| "invalid_port")?;
    std::fs::create_dir_all(&support).map_err(|_| "storage_unavailable")?;
    let lease = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(support.join("daemon.lock"))
        .map_err(|_| "storage_unavailable")?;
    lease.try_lock().map_err(|_| "instance_in_use")?;
    let db = park_storage::web::open(&file).map_err(|_| "storage_unavailable")?;
    let state = App {
        db: Arc::new(db),
        downloads,
        wake: watch::channel(()).0,
        credential_hash: Sha256::digest(credential).into(),
    };
    let app = Router::new()
        .route("/api/v1/progress/snapshot", get(snapshot))
        .route("/api/v1/progress/page", post(turn_page))
        .route("/api/v1/progress/position", post(position))
        .route("/api/v1/history", post(history))
        .route("/api/v1/library/works", get(library::works))
        .route(
            "/api/v1/tavern/assets/{id}/revisions",
            get(library::revisions),
        )
        .route("/api/v1/downloads", get(library::downloads))
        .route("/api/v1/offline/{task}", get(library::offline::describe))
        .route("/api/v1/offline/{task}/files/{id}", get(library::file))
        .route("/api/v1/progress/events", get(events))
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), authenticate))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|_| "listen_failed")?;
    let supervised = std::env::args().nth(1).as_deref() == Some("--stdio");
    let (closed, parent) = tokio::sync::oneshot::channel();
    if supervised {
        use std::io::{Read, Write};
        let port = listener.local_addr().map_err(|_| "listen_failed")?.port();
        writeln!(std::io::stdout().lock(), "{port}").map_err(|_| "control_closed")?;
        std::thread::spawn(move || {
            let _ = std::io::stdin().read(&mut [0u8]);
            let _ = closed.send(());
        });
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! { _ = tokio::signal::ctrl_c() => (), _ = parent, if supervised => () }
        })
        .await
        .map_err(|_| "service_failed")
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    park_domain::install_crash_diagnostics();
    if let Err(code) = serve().await {
        eprintln!("event=daemon_start code={code}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
