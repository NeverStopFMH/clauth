//! `clauth herdr install`'s decision half. `plan_config` is pure text in, text
//! out, which is where the append-only rule either holds or corrupts a config
//! clauth does not own; the subprocess half (herdr's installer, `config check`)
//! is covered by running the command against a real herdr.

use super::*;

/// Every plan this produces has to append onto the file it was planned against
/// and still parse, or the write turns a working herdr config into a broken one.
/// Goes through `with_append`, the same glue `install` uses, so the seam is
/// pinned here rather than reimplemented.
fn appended(existing: &str, key: &str) -> String {
    let plan = plan_config(existing, key).expect("plan");
    let text = with_append(existing, &plan.append);
    toml::from_str::<toml::Value>(&text).expect("appended config parses");
    text
}

#[test]
fn empty_config_gets_both_blocks() {
    let text = appended("", "prefix+a");
    assert!(text.contains(r#"command = "clauth.open""#));
    assert!(text.contains(r#"key = "prefix+a""#));
    assert!(text.contains("[ui.sidebar.agents.rows_by_agent]"));
    assert!(text.contains("$clauth"));
}

#[test]
fn an_existing_binding_is_left_alone() {
    let existing = concat!(
        "[[keys.command]]\n",
        "key = \"prefix+z\"\n",
        "type = \"plugin_action\"\n",
        "command = \"clauth.open\"\n"
    );
    let plan = plan_config(existing, "prefix+a").expect("plan");
    assert!(
        !plan.append.contains("[[keys.command]]"),
        "would double-bind the action"
    );
    assert!(plan.notes.iter().any(|n| n.contains("already bound")));
    // The sidebar half is still missing, so the run is not a no-op.
    assert!(plan.append.contains("rows_by_agent"));
}

#[test]
fn another_plugins_binding_does_not_count_as_ours() {
    let existing = concat!(
        "[[keys.command]]\n",
        "key = \"prefix+g\"\n",
        "type = \"plugin_action\"\n",
        "command = \"someone.else\"\n"
    );
    let text = appended(existing, "prefix+a");
    assert!(text.contains(r#"command = "clauth.open""#));
    // Arrays of tables append cleanly, so both bindings survive.
    let doc: toml::Value = toml::from_str(&text).expect("parses");
    let commands = doc["keys"]["command"].as_array().expect("array");
    assert_eq!(commands.len(), 2);
}

#[test]
fn a_claude_row_already_rendering_the_token_is_left_alone() {
    let existing = concat!(
        "[ui.sidebar.agents.rows_by_agent]\n",
        r#"claude = [["state_icon"], ["agent", "$clauth"]]"#,
        "\n"
    );
    let plan = plan_config(existing, "prefix+a").expect("plan");
    assert!(!plan.append.contains("rows_by_agent"));
    assert!(plan.notes.iter().any(|n| n.contains("already renders")));
}

/// The duplicate-table case: appending our own `[ui.sidebar.agents.rows_by_agent]`
/// beside theirs is a parse error, so the plan has to hand the line over instead.
#[test]
fn a_claude_row_without_the_token_is_reported_never_duplicated() {
    let existing = concat!(
        "[ui.sidebar.agents.rows_by_agent]\n",
        r#"claude = [["state_icon"], ["agent"]]"#,
        "\n"
    );
    let plan = plan_config(existing, "prefix+a").expect("plan");
    assert!(
        !plan.append.contains("rows_by_agent"),
        "would duplicate the table"
    );
    assert!(
        plan.notes
            .iter()
            .any(|n| n.contains("already sets a claude row"))
    );
    appended(existing, "prefix+a");
}

#[test]
fn a_rows_by_agent_table_for_other_agents_is_reported_never_duplicated() {
    let existing = concat!(
        "[ui.sidebar.agents.rows_by_agent]\n",
        r#"codex = [["state_icon"], ["agent"]]"#,
        "\n"
    );
    let plan = plan_config(existing, "prefix+a").expect("plan");
    assert!(
        !plan.append.contains("rows_by_agent"),
        "would duplicate the table"
    );
    assert!(plan.notes.iter().any(|n| n.contains("covers other agents")));
    appended(existing, "prefix+a");
}

/// `[ui.sidebar.agents]` existing without `rows_by_agent` is the common shape
/// (someone who set `row_gap`), and appending the child table is legal there.
#[test]
fn a_sidebar_agents_table_without_rows_by_agent_still_gets_the_block() {
    let existing = "[ui.sidebar.agents]\nrow_gap = 1\n";
    let text = appended(existing, "prefix+a");
    assert!(text.contains("[ui.sidebar.agents.rows_by_agent]"));
    let doc: toml::Value = toml::from_str(&text).expect("parses");
    assert_eq!(
        doc["ui"]["sidebar"]["agents"]["row_gap"].as_integer(),
        Some(1)
    );
    assert!(
        doc["ui"]["sidebar"]["agents"]["rows_by_agent"]
            .get("claude")
            .is_some()
    );
}

#[test]
fn a_fully_wired_config_plans_nothing() {
    let existing = appended("", "prefix+a");
    let plan = plan_config(&existing, "prefix+a").expect("plan");
    assert!(plan.append.is_empty(), "second run would append again");
    assert_eq!(plan.notes.len(), 2);
}

#[test]
fn a_config_that_does_not_parse_fails_before_anything_is_written() {
    assert!(plan_config("this is not toml", "prefix+a").is_err());
}

/// Comments and unrelated keys survive, because the write appends text rather
/// than reserializing a parsed document.
#[test]
fn unrelated_config_survives_verbatim() {
    let existing = "# my herdr config\n[ui]\naccent = \"cyan\"\n";
    let text = appended(existing, "prefix+a");
    assert!(text.starts_with(existing));
    assert!(text.contains("# my herdr config"));
}

#[test]
fn a_key_that_would_break_the_file_is_refused() {
    assert!(validate_key("prefix+a").is_ok());
    assert!(
        validate_key(r#"a" , x = ""#).is_err(),
        "a quote would escape the TOML string"
    );
    assert!(validate_key("a\\b").is_err());
    assert!(validate_key("a\nb").is_err());
    assert!(validate_key("").is_err());
    assert!(validate_key(&"x".repeat(65)).is_err());
}

/// The token search walks the row structure, so a rendering change in the toml
/// crate cannot silently turn "already wired" into "wire it again".
#[test]
fn token_detection_walks_nested_groups() {
    let row: toml::Value = toml::from_str(r#"v = [["a"], ["b", "$clauth"]]"#).expect("parse");
    assert!(mentions_token(&row["v"]));
    let plain: toml::Value = toml::from_str(r#"v = [["a"], ["b"]]"#).expect("parse");
    assert!(!mentions_token(&plain["v"]));
    let substring: toml::Value = toml::from_str(r#"v = ["$clauthx"]"#).expect("parse");
    assert!(
        !mentions_token(&substring["v"]),
        "a longer name is a different token"
    );
}

/// `with_append` is the seam between the plan and the file. A config that ends
/// mid-line would otherwise take the first appended line onto that line.
#[test]
fn the_append_seam_never_joins_two_lines() {
    assert_eq!(with_append("a = 1", "\n[b]\n"), "a = 1\n\n[b]\n");
    assert_eq!(with_append("a = 1\n", "\n[b]\n"), "a = 1\n\n[b]\n");
    assert_eq!(with_append("", "\n[b]\n"), "\n[b]\n");
    let joined = with_append(
        "accent = \"cyan\"",
        &plan_config("accent = \"cyan\"", "prefix+a")
            .expect("plan")
            .append,
    );
    toml::from_str::<toml::Value>(&joined).expect("a config with no trailing newline still parses");
}

/// Spellings that parse to the same shape but cannot be extended by appending a
/// header. Walking the parsed tree cannot tell these from an absent table, so
/// the plan has to try the text and hand the block over when it does not hold.
#[test]
fn a_table_spelled_inline_is_handed_over_never_appended_onto() {
    for existing in [
        r#"ui = { accent = "cyan" }"#,
        r#"keys = { }"#,
        "[keys.command]\nkey = \"prefix+z\"\n",
        r#"keys.command = [{ key = "prefix+z", type = "shell", command = "ls" }]"#,
        "[ui.sidebar.agents]\nrows_by_agent = { codex = [[\"agent\"]] }\n",
    ] {
        let plan = plan_config(existing, "prefix+a").expect("plan");
        let text = with_append(existing, &plan.append);
        toml::from_str::<toml::Value>(&text)
            .unwrap_or_else(|e| panic!("appending onto {existing:?} broke the config: {e}"));
        assert!(
            !plan.notes.is_empty(),
            "{existing:?} was silently left unwired with no note"
        );
    }
}

/// The refusal set is what `write_validated` bails on, so a complaint the config
/// already carried must never land in it.
#[test]
fn only_diagnostics_the_edit_added_are_refused() {
    let before = vec!["unknown config key accent".to_string()];
    let after = vec![
        "unknown config key accent".to_string(),
        "invalid keybinding: keys.command[0].key".to_string(),
    ];
    assert_eq!(
        added_diagnostics(&before, &after),
        vec!["invalid keybinding: keys.command[0].key"]
    );
    assert!(added_diagnostics(&before, &before).is_empty());
    assert!(
        added_diagnostics(&after, &before).is_empty(),
        "a complaint that went away is not ours"
    );
    let twice = vec!["same".to_string(), "same".to_string()];
    assert_eq!(
        added_diagnostics(&[], &twice),
        vec!["same"],
        "one line, reported once"
    );
}

/// herdr prints `<root>/plugins/config/<component>`; anything shorter is not
/// that shape, and guessing a root writes a config herdr never reads.
#[test]
fn the_config_root_is_derived_from_herdrs_own_path_or_refused() {
    assert_eq!(
        config_path_from_plugin_dir("/home/u/.config/herdr/plugins/config/clauth"),
        Some(std::path::PathBuf::from(
            "/home/u/.config/herdr/config.toml"
        ))
    );
    assert_eq!(
        config_path_from_plugin_dir(
            "/Users/u/Library/Application Support/herdr/plugins/config/clauth"
        ),
        Some(std::path::PathBuf::from(
            "/Users/u/Library/Application Support/herdr/config.toml"
        ))
    );
    assert_eq!(config_path_from_plugin_dir(""), None);
    assert_eq!(
        config_path_from_plugin_dir("/plugins/config/clauth"),
        None,
        "root has no config.toml of herdr's"
    );
    assert_eq!(config_path_from_plugin_dir("clauth"), None);
}
