//! `POST /api/login/oauth` and `POST /api/profiles/{name}/login/alibaba` —
//! the two Setup-tab actions that open a browser and block on a loopback
//! callback, wrapped as fire-and-poll jobs (see [`super::jobs`]) since
//! neither can finish inside one HTTP request.

use std::sync::Arc;

use serde::Deserialize;
use tiny_http::StatusCode;

use super::jobs::{self, JobStore};
use super::{RouteResult, error_body, read_json_body};
use crate::profile::{ConfigHandle, ProfileName};

#[derive(Deserialize)]
struct OauthLoginRequest {
    name: String,
    #[serde(default)]
    model: Option<String>,
}

/// `POST /api/login/oauth` `{name, model?}` — adds a brand-new account via
/// browser OAuth, mirroring `actions::create_profile_from_login`. The name
/// is validated up front (same guard `profiles::create` uses) so a doomed
/// request never opens a browser at all; everything past that runs on a
/// background thread and reports through the job it returns.
pub(super) fn start_oauth(
    config: &ConfigHandle,
    jobs_store: &JobStore,
    request: &mut tiny_http::Request,
) -> RouteResult {
    let body: OauthLoginRequest = read_json_body(request)?;
    {
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let cfg = config.lock().expect("config mutex poisoned");
        let existing: Vec<&str> = cfg.profiles.iter().map(|p| p.name.as_str()).collect();
        if let Err(e) = crate::actions::validate_profile_name(&body.name, &existing, None) {
            return Err((StatusCode(422), error_body(&e.to_string())));
        }
    }

    let job_id = jobs::start(jobs_store);
    let thread_job_id = job_id.clone();
    let config = Arc::clone(config);
    let jobs_store = Arc::clone(jobs_store);
    let OauthLoginRequest { name, model } = body;
    std::thread::spawn(move || {
        let outcome = match crate::oauth_login::login_with(|_progress| {}) {
            Ok(outcome) => {
                #[allow(
                    clippy::expect_used,
                    reason = "config mutex poisoning is unrecoverable"
                )]
                let mut cfg = config.lock().expect("config mutex poisoned");
                crate::actions::create_profile_from_login(
                    &mut cfg,
                    name.clone(),
                    model,
                    outcome.credentials,
                    outcome.account_uuid,
                )
                .map(|()| serde_json::json!({"name": name}))
                .map_err(|e| e.to_string())
            }
            Err(e) => Err(e.user_message()),
        };
        jobs::finish(&jobs_store, &thread_job_id, outcome);
    });

    Ok((
        StatusCode(200),
        serde_json::json!({"job_id": job_id}).to_string(),
    ))
}

#[derive(Deserialize)]
struct AlibabaLoginRequest {
    site: String,
    region: String,
}

/// `POST /api/profiles/{name}/login/alibaba` `{site, region}` — captures a
/// console session onto an EXISTING profile (unlike OAuth, this never
/// creates one), mirroring `actions::store_console_login`.
pub(super) fn start_alibaba(
    config: &ConfigHandle,
    jobs_store: &JobStore,
    name: &str,
    request: &mut tiny_http::Request,
) -> RouteResult {
    let body: AlibabaLoginRequest = read_json_body(request)?;
    let Some(site) = crate::profile::ConsoleSite::parse(&body.site) else {
        return Err((StatusCode(400), error_body("invalid site")));
    };
    let name = ProfileName::from(name.to_string());
    {
        #[allow(
            clippy::expect_used,
            reason = "config mutex poisoning is unrecoverable"
        )]
        let cfg = config.lock().expect("config mutex poisoned");
        if cfg.find(&name).is_none() {
            return Err((StatusCode(404), error_body("profile not found")));
        }
    }

    let job_id = jobs::start(jobs_store);
    let thread_job_id = job_id.clone();
    let config = Arc::clone(config);
    let jobs_store = Arc::clone(jobs_store);
    let region = body.region;
    std::thread::spawn(move || {
        let outcome = match crate::alibaba_login::login_with(site, &region, |_url| {}) {
            Ok(outcome) => {
                #[allow(
                    clippy::expect_used,
                    reason = "config mutex poisoning is unrecoverable"
                )]
                let mut cfg = config.lock().expect("config mutex poisoned");
                crate::actions::store_console_login(&mut cfg, &name, outcome.console)
                    .map(|()| serde_json::json!({"name": name.to_string()}))
                    .map_err(|e| e.to_string())
            }
            Err(e) => Err(e.to_string()),
        };
        jobs::finish(&jobs_store, &thread_job_id, outcome);
    });

    Ok((
        StatusCode(200),
        serde_json::json!({"job_id": job_id}).to_string(),
    ))
}

#[cfg(test)]
#[path = "../../tests/inline/web_login.rs"]
mod tests;
