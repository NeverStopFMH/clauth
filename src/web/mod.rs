//! The dashboard's embedded HTTP server: runs inside `clauth daemon`, bound
//! to 127.0.0.1 only. Read endpoints are open; write endpoints require the
//! bearer token from [`auth::load_or_create_token`]. See
//! `docs/superpowers/specs/2026-08-31-web-dashboard-backend-api-design.md`
//! for the full design.

mod auth;
mod config;
mod fallback;
mod jobs;
mod login;
mod plugin;
mod profiles;

use std::sync::Arc;

use serde::de::DeserializeOwned;
use tiny_http::{Header, Method, Response, Server, StatusCode};

use crate::profile::ConfigHandle;

pub(crate) use auth::load_or_create_token;

/// Default port the dashboard listens on. Callers that need a different
/// bind target (tests use `127.0.0.1:0` for an OS-assigned free port) pass
/// their own address to [`spawn`] instead of using this.
pub(crate) const DEFAULT_PORT: u16 = 47893;

/// A status code + JSON body — every route handler's return type. `Result`'s
/// two arms are used purely for `?`-early-return convenience (a body-parse
/// failure short-circuits past the business logic); [`resolve`] collapses
/// both arms back into one, since a failure response is exactly as valid an
/// HTTP response as a success one.
pub(super) type RouteResult = Result<(StatusCode, String), (StatusCode, String)>;

fn resolve(result: RouteResult) -> (StatusCode, String) {
    match result {
        Ok(r) | Err(r) => r,
    }
}

/// `{"ok":true}`, the shape every successful write endpoint answers with —
/// none of them have a natural payload to return beyond "it worked".
pub(super) fn ok_body() -> String {
    r#"{"ok":true}"#.to_string()
}

/// `{"error":"<msg>"}`, JSON-escaping `msg` so an error string that happens
/// to contain a `"` or newline (an account name, a filesystem path) can't
/// produce invalid JSON.
pub(super) fn error_body(msg: &str) -> String {
    serde_json::json!({ "error": msg }).to_string()
}

/// Parse the request body as JSON, or a 400 response reader/handlers `?`
/// straight out of the route function.
pub(super) fn read_json_body<T: DeserializeOwned>(
    request: &mut tiny_http::Request,
) -> Result<T, (StatusCode, String)> {
    let mut body = String::new();
    request.as_reader().read_to_string(&mut body).map_err(|e| {
        (
            StatusCode(400),
            error_body(&format!("failed to read body: {e}")),
        )
    })?;
    serde_json::from_str(&body).map_err(|e| {
        (
            StatusCode(400),
            error_body(&format!("invalid JSON body: {e}")),
        )
    })
}

/// A running server plus the means to stop it. [`Handle::stop`] breaks the
/// accept loop out of its blocking `recv` and joins the thread, so tests that
/// spawn a fresh server per case never accumulate leaked threads across the
/// suite.
pub(crate) struct Handle {
    #[allow(
        dead_code,
        reason = "read by Handle::addr, currently only called from tests; \
                  production spawns and drops the handle, the accept thread's \
                  own Arc clone keeps the server alive for the process lifetime"
    )]
    server: Arc<Server>,
    #[allow(
        dead_code,
        reason = "read by Handle::stop, currently only called from tests"
    )]
    join: Option<std::thread::JoinHandle<()>>,
}

impl Handle {
    /// The address actually bound — the real port when the caller asked for
    /// an OS-assigned one (`:0`). Test-only today (production always binds
    /// the well-known [`DEFAULT_PORT`] and has no need to read it back).
    #[allow(dead_code, reason = "only called from tests today")]
    #[allow(
        clippy::expect_used,
        reason = "this crate only ever binds tiny_http's TCP listener, never a unix socket"
    )]
    pub(crate) fn addr(&self) -> std::net::SocketAddr {
        self.server
            .server_addr()
            .to_ip()
            .expect("always an IP: this crate never binds a unix socket")
    }

    /// Stop the accept loop and wait for the thread to exit. Test-only today
    /// — production lets the server run for the daemon's whole lifetime.
    #[allow(dead_code, reason = "only called from tests today")]
    pub(crate) fn stop(mut self) {
        self.server.unblock();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Start the server on a background thread. `addr` is anything
/// `tiny_http::Server::http` accepts (`"127.0.0.1:47893"`, or `"127.0.0.1:0"`
/// for tests). Returns once the socket is bound, so a caller can read
/// [`Handle::addr`] immediately without racing the accept loop's startup.
pub(crate) fn spawn(config: ConfigHandle, token: String, addr: &str) -> std::io::Result<Handle> {
    let server = Arc::new(
        Server::http(addr).map_err(|e| std::io::Error::other(format!("web server bind: {e}")))?,
    );
    let accept_server = Arc::clone(&server);
    let jobs_store = jobs::new_store();
    let join = std::thread::spawn(move || {
        for request in accept_server.incoming_requests() {
            handle_request(&config, &jobs_store, &token, request);
        }
    });
    Ok(Handle {
        server,
        join: Some(join),
    })
}

/// Route one request. Read routes answer unconditionally; write routes 401
/// without a valid bearer token, checked BEFORE the route body ever runs.
fn handle_request(
    config: &ConfigHandle,
    jobs_store: &jobs::JobStore,
    token: &str,
    mut request: tiny_http::Request,
) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let (status, body) = if is_write_method(&method) && !request_is_authorized(&request, token) {
        (StatusCode(401), error_body("unauthorized"))
    } else {
        resolve(route(config, jobs_store, &method, &url, &mut request))
    };
    let _ = request.respond(json_response(status, &body));
}

fn route(
    config: &ConfigHandle,
    jobs_store: &jobs::JobStore,
    method: &Method,
    url: &str,
    request: &mut tiny_http::Request,
) -> RouteResult {
    let path = url.split('?').next().unwrap_or(url);
    match (method, path) {
        (Method::Get, "/api/health") => Ok((StatusCode(200), r#"{"ok":true}"#.to_string())),
        (Method::Get, "/api/status") => Ok(status_body()),
        (Method::Get, "/api/status/incidents") => Ok(incidents_body()),
        (Method::Post, "/api/profiles/switch") => profiles::switch(config, request),
        (Method::Post, "/api/profiles/reorder") => profiles::reorder(config, request),
        (Method::Post, "/api/profiles") => profiles::create(config, request),
        (Method::Post, "/api/login/oauth") => login::start_oauth(config, jobs_store, request),
        (Method::Patch, "/api/fallback") => fallback::set_chain(config, request),
        (Method::Patch, "/api/config") => config::patch(config, request),
        (Method::Get, "/api/plugin/status") => Ok(plugin::status()),
        (Method::Post, "/api/plugin/install") => Ok(plugin::install()),
        (Method::Post, "/api/plugin/self-heal") => Ok(plugin::self_heal()),
        (Method::Get, p) if p.starts_with("/api/jobs/") => {
            Ok(jobs::poll(jobs_store, path_tail(p, "/api/jobs/")))
        }
        (Method::Delete, p) if p.starts_with("/api/profiles/") => {
            profiles::delete(config, path_tail(p, "/api/profiles/"), url)
        }
        (Method::Patch, p) if p.ends_with("/fallback") && p.starts_with("/api/profiles/") => {
            let name = path_tail(p, "/api/profiles/")
                .strip_suffix("/fallback")
                .unwrap_or_default();
            fallback::patch_member(config, name, request)
        }
        (Method::Post, p) if p.ends_with("/login/alibaba") && p.starts_with("/api/profiles/") => {
            let name = path_tail(p, "/api/profiles/")
                .strip_suffix("/login/alibaba")
                .unwrap_or_default();
            login::start_alibaba(config, jobs_store, name, request)
        }
        (Method::Patch, p) if p.starts_with("/api/profiles/") => {
            profiles::patch(config, path_tail(p, "/api/profiles/"), request)
        }
        _ => Err((StatusCode(404), error_body("not found"))),
    }
}

fn is_write_method(method: &Method) -> bool {
    matches!(method, Method::Post | Method::Patch | Method::Delete)
}

/// The segment of `path` after `prefix` — the `{name}` in `/api/profiles/{name}`.
/// Every character `validate_profile_name` allows (`[A-Za-z0-9._@+-]`) is a
/// plain, unreserved URL path character with no percent-encoding, so no
/// decoding step is needed here.
fn path_tail<'a>(path: &'a str, prefix: &str) -> &'a str {
    path.strip_prefix(prefix).unwrap_or(path)
}

/// `GET /api/status`: the same `~/.clauth/status.json` a daemon publishes
/// every tick (`wiki/Daemon.md` is the read contract), served verbatim rather
/// than rebuilt here — the daemon is already the single writer of that JSON,
/// so reading its own file keeps this endpoint from ever drifting from what
/// `clauth status --json` and any other reader of the file already see. A
/// 503 (not 404) when the file doesn't exist yet: the server binds before
/// the first tick writes it, a startup window rather than a missing route.
fn status_body() -> (StatusCode, String) {
    match read_status_file() {
        Ok(body) => (StatusCode(200), body),
        Err(_) => (StatusCode(503), error_body("status not ready yet")),
    }
}

fn read_status_file() -> std::io::Result<String> {
    let dir = crate::profile::clauth_dir().map_err(std::io::Error::other)?;
    std::fs::read_to_string(dir.join(crate::daemon::STATUS_FILE))
}

/// `GET /api/status/incidents`: `~/.clauth/status_cache.json` verbatim — the
/// Claude status-page feed's local cache (`{"fetched_at_ms", "incidents"}`),
/// same "serve the file the background writer already maintains" pattern as
/// [`status_body`]. 503 while nothing has been fetched yet (a fresh install,
/// or the poller hasn't completed its first round).
fn incidents_body() -> (StatusCode, String) {
    let body = crate::status::cache_path()
        .ok_or(())
        .and_then(|path| std::fs::read_to_string(path).map_err(|_| ()));
    match body {
        Ok(body) => (StatusCode(200), body),
        Err(()) => (StatusCode(503), error_body("no status feed cached yet")),
    }
}

fn request_is_authorized(request: &tiny_http::Request, token: &str) -> bool {
    let header = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .map(|h| h.value.as_str());
    auth::check_bearer(header, token)
}

#[allow(
    clippy::expect_used,
    reason = "the header name/value are both compile-time-constant ASCII literals"
)]
fn json_response(status: StatusCode, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
        .expect("static header name/value are always valid");
    Response::from_string(body.to_string())
        .with_status_code(status)
        .with_header(header)
}

#[cfg(test)]
#[path = "../../tests/inline/web_mod.rs"]
mod tests;
