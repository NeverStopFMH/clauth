#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(unsafe_code)]

//! Coverage for the `format` argument every tool now takes: prose by default,
//! JSON by opt-in, an unrecognised value refused by name, and exactly one
//! content block in either spelling (the old live-usage footer is folded in).

use super::*;
use crate::testutil::HomeSandbox;

fn drive<F>(fut: F) -> CallToolResult
where
    F: std::future::Future<Output = Result<CallToolResult, ErrorData>>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    rt.block_on(fut)
        .expect("tool returns a tool result, never a transport error")
}

fn first_text(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.as_text())
        .map(|t| t.text.clone())
        .expect("first content block is text")
}

fn key_set(body: &serde_json::Value) -> std::collections::BTreeSet<&str> {
    body.as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect()
}

fn assert_refusal(result: &CallToolResult, bad: &str) {
    assert_eq!(
        result.is_error,
        Some(true),
        "an unrecognised format is a tool error"
    );
    assert_eq!(
        result.content.len(),
        1,
        "the refusal is a single content block"
    );
    let body: serde_json::Value =
        serde_json::from_str(&first_text(result)).expect("refusal is JSON");
    assert_eq!(body["ok"], serde_json::Value::Bool(false));
    assert_eq!(
        body["reason"],
        serde_json::Value::String(format!(
            "unrecognized format \"{bad}\": accepted \"prose\" and \"json\""
        ))
    );
}

#[test]
fn every_tool_refuses_an_unrecognised_format_by_name() {
    let server = ClauthServer::new();

    assert_refusal(
        &drive(server.list_profiles(Parameters(ListProfilesArgs {
            names: None,
            format: Some("yaml".to_string()),
        }))),
        "yaml",
    );
    assert_refusal(
        &drive(server.which(Parameters(WhichArgs {
            format: Some("yaml".to_string()),
        }))),
        "yaml",
    );
    assert_refusal(
        &drive(server.switch(Parameters(SwitchArgs {
            name: "any".to_string(),
            format: Some("yaml".to_string()),
        }))),
        "yaml",
    );
    assert_refusal(
        &drive(server.delegate(Parameters(DelegateArgs {
            profile: Some("any".to_string()),
            profiles: None,
            prompt: Some("hi".to_string()),
            prompt_file: None,
            model: None,
            cwd: None,
            env: None,
            args: None,
            timeout_secs: None,
            idle_secs: None,
            resume: None,
            isolated: None,
            background: None,
            monitor: None,
            format: Some("yaml".to_string()),
        }))),
        "yaml",
    );
    assert_refusal(
        &drive(server.delegate_result(Parameters(DelegateResultArgs {
            job_id: "d-1".to_string(),
            wait_secs: None,
            format: Some("yaml".to_string()),
        }))),
        "yaml",
    );
}

#[test]
fn which_prose_default_is_one_block_and_json_keeps_the_old_keys() {
    let _home = HomeSandbox::new();

    let prose = drive(ClauthServer::new().which(Parameters(WhichArgs { format: None })));
    assert_eq!(prose.content.len(), 1, "prose is a single content block");
    let text = first_text(&prose);
    assert!(
        serde_json::from_str::<serde_json::Value>(&text).is_err(),
        "the prose default must not be JSON"
    );
    assert_eq!(
        text,
        "session profile unknown, source unknown, tier unknown; active profile `unknown`: 5h unknown, 7d unknown"
    );

    let json = drive(ClauthServer::new().which(Parameters(WhichArgs {
        format: Some("json".to_string()),
    })));
    assert_eq!(json.content.len(), 1, "json is a single content block");
    let body: serde_json::Value = serde_json::from_str(&first_text(&json)).expect("json payload");
    assert_eq!(
        key_set(&body),
        ["live_usage", "profile", "source", "throughput", "tier"]
            .into_iter()
            .collect(),
        "the JSON spelling is the old payload plus the folded-in live_usage, nothing else"
    );
    assert_eq!(
        key_set(&body["live_usage"]),
        ["5h_used_pct", "7d_used_pct", "profile"]
            .into_iter()
            .collect(),
        "the footer folds in as the three live_usage fields"
    );
}

#[test]
fn list_profiles_prose_default_and_json_shape() {
    let _home = HomeSandbox::new();

    let prose = drive(
        ClauthServer::new().list_profiles(Parameters(ListProfilesArgs {
            names: None,
            format: None,
        })),
    );
    assert_eq!(prose.content.len(), 1, "prose is a single content block");
    assert_eq!(first_text(&prose), "no profiles");

    let json = drive(
        ClauthServer::new().list_profiles(Parameters(ListProfilesArgs {
            names: None,
            format: Some("json".to_string()),
        })),
    );
    assert_eq!(json.content.len(), 1, "json is a single content block");
    let body: serde_json::Value = serde_json::from_str(&first_text(&json)).expect("json payload");
    assert_eq!(
        key_set(&body),
        ["profiles"].into_iter().collect(),
        "list_profiles has no live-usage footer, so its top level stays exactly `profiles`"
    );
}

#[test]
fn switch_prose_default_and_json_error_keys() {
    let _home = HomeSandbox::new();

    let prose = drive(ClauthServer::new().switch(Parameters(SwitchArgs {
        name: "ghost".to_string(),
        format: None,
    })));
    assert_eq!(prose.is_error, Some(true));
    assert_eq!(prose.content.len(), 1, "prose is a single content block");
    assert_eq!(
        first_text(&prose),
        "switch failed: profile not found: ghost; active profile `unknown`: 5h unknown, 7d unknown"
    );

    let json = drive(ClauthServer::new().switch(Parameters(SwitchArgs {
        name: "ghost".to_string(),
        format: Some("json".to_string()),
    })));
    assert_eq!(json.content.len(), 1, "json is a single content block");
    let body: serde_json::Value = serde_json::from_str(&first_text(&json)).expect("json payload");
    assert_eq!(
        key_set(&body),
        ["live_usage", "ok", "reason"].into_iter().collect(),
        "the switch error envelope keeps its two keys and folds in live_usage"
    );
}

#[test]
fn delegate_depth_prose_default_and_json_keys() {
    let _guard = crate::profile::HOME_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let saved = std::env::var(MCP_DEPTH_ENV).ok();
    // SAFETY: test-only, serialized by the lock above, restored unconditionally.
    unsafe { std::env::set_var(MCP_DEPTH_ENV, "1") };

    let args = |format| {
        Parameters(DelegateArgs {
            profile: Some("any".to_string()),
            profiles: None,
            prompt: Some("hi".to_string()),
            prompt_file: None,
            model: None,
            cwd: None,
            env: None,
            args: None,
            timeout_secs: None,
            idle_secs: None,
            resume: None,
            isolated: None,
            background: None,
            monitor: None,
            format,
        })
    };

    let prose = drive(ClauthServer::new().delegate(args(None)));
    assert_eq!(prose.is_error, Some(true));
    assert_eq!(prose.content.len(), 1, "prose is a single content block");
    assert_eq!(
        first_text(&prose),
        "delegate to `any` failed: delegation depth exceeded (max 1)"
    );

    let json = drive(ClauthServer::new().delegate(args(Some("json".to_string()))));
    assert_eq!(json.content.len(), 1, "json is a single content block");
    let body: serde_json::Value = serde_json::from_str(&first_text(&json)).expect("json payload");
    assert_eq!(
        key_set(&body),
        ["is_error", "profile", "result"].into_iter().collect(),
        "the depth refusal has no live usage to fold in, so it keeps its three keys"
    );

    // SAFETY: same as above — restore the prior value.
    unsafe {
        match &saved {
            Some(v) => std::env::set_var(MCP_DEPTH_ENV, v),
            None => std::env::remove_var(MCP_DEPTH_ENV),
        }
    }
}

#[test]
fn delegate_result_invalid_prose_default_and_json_keys() {
    let prose = drive(
        ClauthServer::new().delegate_result(Parameters(DelegateResultArgs {
            job_id: "../evil".to_string(),
            wait_secs: None,
            format: None,
        })),
    );
    assert_eq!(prose.is_error, Some(true));
    assert_eq!(prose.content.len(), 1, "prose is a single content block");
    assert_eq!(first_text(&prose), "error: invalid job_id");

    let json = drive(
        ClauthServer::new().delegate_result(Parameters(DelegateResultArgs {
            job_id: "../evil".to_string(),
            wait_secs: None,
            format: Some("json".to_string()),
        })),
    );
    assert_eq!(json.content.len(), 1, "json is a single content block");
    let body: serde_json::Value = serde_json::from_str(&first_text(&json)).expect("json payload");
    assert_eq!(
        key_set(&body),
        ["is_error", "result"].into_iter().collect(),
        "the invalid-job_id envelope keeps its two keys"
    );
}
