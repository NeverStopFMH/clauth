//! The dashboard's embedded HTTP server: runs inside `clauth daemon`, bound
//! to 127.0.0.1 only. Read endpoints are open; write endpoints require the
//! bearer token from [`auth::load_or_create_token`]. See
//! `docs/superpowers/specs/2026-08-31-web-dashboard-backend-api-design.md`
//! for the full design.

mod auth;

use std::sync::Arc;

use tiny_http::{Header, Method, Response, Server, StatusCode};

use crate::profile::ConfigHandle;

pub(crate) use auth::load_or_create_token;

/// Default port the dashboard listens on. Callers that need a different
/// bind target (tests use `127.0.0.1:0` for an OS-assigned free port) pass
/// their own address to [`spawn`] instead of using this.
pub(crate) const DEFAULT_PORT: u16 = 47893;

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
    let join = std::thread::spawn(move || {
        for request in accept_server.incoming_requests() {
            handle_request(&config, &token, request);
        }
    });
    Ok(Handle {
        server,
        join: Some(join),
    })
}

/// Route one request. Read routes answer unconditionally; write routes 401
/// without a valid bearer token. Only `GET /api/health` exists so far — the
/// rest of the API lands in follow-up slices (see the design spec).
fn handle_request(_config: &ConfigHandle, token: &str, request: tiny_http::Request) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let response = match (&method, url.as_str()) {
        (Method::Get, "/api/health") => json_response(StatusCode(200), r#"{"ok":true}"#),
        _ if is_write_method(&method) && !request_is_authorized(&request, token) => {
            json_response(StatusCode(401), r#"{"error":"unauthorized"}"#)
        }
        _ => json_response(StatusCode(404), r#"{"error":"not found"}"#),
    };
    let _ = request.respond(response);
}

fn is_write_method(method: &Method) -> bool {
    matches!(method, Method::Post | Method::Patch | Method::Delete)
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
