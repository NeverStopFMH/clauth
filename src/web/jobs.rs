//! In-memory async job tracking for the two write endpoints that can't
//! finish inside one request/response: OAuth and Alibaba console login both
//! block on a browser round trip (up to their own multi-minute timeouts).
//! Deliberately NOT the MCP `delegate` job system in `mcp/jobs.rs` — that
//! one is disk-persisted and shaped around a spawned `claude` subprocess
//! (pid, heartbeat, streamed output, session id), none of which a login flow
//! has. Jobs here live only for the daemon process's lifetime: a login
//! either completes within its own timeout or the caller gives up and
//! starts a fresh one — nothing here needs to survive a restart.

use std::collections::HashMap;
use std::sync::Arc;

use tiny_http::StatusCode;

use super::error_body;
use crate::lockorder::RankedMutex;
use crate::lockorder::rank::WebJobs;

#[derive(Clone)]
pub(super) enum JobStatus {
    Pending,
    Succeeded(serde_json::Value),
    Failed(String),
}

pub(super) type JobStore = Arc<RankedMutex<HashMap<String, JobStatus>, WebJobs>>;

pub(super) fn new_store() -> JobStore {
    Arc::new(RankedMutex::new(HashMap::new()))
}

/// 16 random bytes, hex-encoded — long enough that two jobs never collide,
/// short enough to sit in a URL path segment.
fn new_job_id() -> String {
    let mut bytes = [0u8; 16];
    // A CSPRNG failure here would be a broken host, not a job-store problem;
    // falling back to zeroed bytes is safe because a collision only ever
    // costs a 404 on the SECOND job it would apply to, never data loss.
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Register a pending job and return its id.
pub(super) fn start(store: &JobStore) -> String {
    let id = new_job_id();
    #[allow(
        clippy::expect_used,
        reason = "job store mutex poisoning is unrecoverable"
    )]
    store
        .lock()
        .expect("job store poisoned")
        .insert(id.clone(), JobStatus::Pending);
    id
}

/// Publish a job's final outcome. Called once, from the background thread
/// that ran the login flow.
pub(super) fn finish(store: &JobStore, id: &str, result: Result<serde_json::Value, String>) {
    let status = match result {
        Ok(v) => JobStatus::Succeeded(v),
        Err(e) => JobStatus::Failed(e),
    };
    #[allow(
        clippy::expect_used,
        reason = "job store mutex poisoning is unrecoverable"
    )]
    store
        .lock()
        .expect("job store poisoned")
        .insert(id.to_string(), status);
}

/// `GET /api/jobs/{id}`.
pub(super) fn poll(store: &JobStore, id: &str) -> (StatusCode, String) {
    #[allow(
        clippy::expect_used,
        reason = "job store mutex poisoning is unrecoverable"
    )]
    let jobs = store.lock().expect("job store poisoned");
    match jobs.get(id) {
        None => (StatusCode(404), error_body("job not found")),
        Some(JobStatus::Pending) => (StatusCode(200), r#"{"status":"pending"}"#.to_string()),
        Some(JobStatus::Succeeded(v)) => (
            StatusCode(200),
            serde_json::json!({"status": "succeeded", "result": v}).to_string(),
        ),
        Some(JobStatus::Failed(e)) => (
            StatusCode(200),
            serde_json::json!({"status": "failed", "error": e}).to_string(),
        ),
    }
}

#[cfg(test)]
#[path = "../../tests/inline/web_jobs.rs"]
mod tests;
