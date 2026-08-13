//! Plugin-tab `herdr` row render tests: the dot color carries the verdict, the
//! selector row right-aligns a `[f]` marker exactly when the check offers a fix.
//! The verdict logic itself is unit-tested in `tests/inline/tui_app.rs`; these
//! pin the render (dot hue + marker) per drift state.

use crate::herdr::{ConfigStatus, HerdrProbe, RegistryEntry, SidebarState};
use crate::profile::{AppConfig, AppState};
use crate::tui::app::{App, Check, Health, herdr_check};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use std::path::PathBuf;

const W: u16 = 100;
const H: u16 = 24;

fn entry(enabled: bool, min: Option<&str>, warnings: Vec<&str>) -> RegistryEntry {
    RegistryEntry {
        enabled,
        version: Some("0.1.0".into()),
        min_herdr_version: min.map(str::to_string),
        plugin_root: None,
        source_kind: Some("github".into()),
        warnings: warnings.into_iter().map(str::to_string).collect(),
    }
}

fn probe(version: Option<&str>, entry: Option<RegistryEntry>, error: Option<&str>) -> HerdrProbe {
    HerdrProbe {
        version: version.map(str::to_string),
        entry,
        config_path: Some(PathBuf::from("/tmp/herdr/config.toml")),
        error: error.map(str::to_string),
    }
}

fn config(parsed: bool, key: Option<&str>, sidebar: SidebarState) -> ConfigStatus {
    ConfigStatus {
        parsed,
        bound_key: key.map(str::to_string),
        sidebar,
    }
}

fn healthy_probe() -> HerdrProbe {
    probe(
        Some("0.8.0"),
        Some(entry(true, Some("0.8.0"), vec![])),
        None,
    )
}

fn healthy_config() -> ConfigStatus {
    config(true, Some("prefix+a"), SidebarState::Templated)
}

fn app_with(check: Check) -> App {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.plugin.checks = vec![check];
    app.plugin.cursor = 0;
    app
}

fn render(app: &App) -> (Vec<String>, ratatui::buffer::Buffer) {
    let mut term = Terminal::new(TestBackend::new(W, H)).unwrap();
    term.draw(|f| super::draw(f, f.area(), app)).unwrap();
    let buf = term.backend().buffer().clone();
    (crate::testutil::buffer_rows(&buf), buf)
}

/// The dot carries the verdict hue and the selector row shows `[f]` exactly when
/// the check offers one. `expected` is the health the state should render.
fn assert_row(check: Check, expected: Health, expect_fix: bool) {
    assert_eq!(
        check.fix.is_some(),
        expect_fix,
        "fix offer for {:?}",
        check.detail
    );
    let app = app_with(check);
    let (rows, buf) = render(&app);
    let row_idx = rows
        .iter()
        .position(|r| r.contains("● herdr"))
        .unwrap_or_else(|| panic!("no herdr selector row:\n{}", rows.join("\n")));
    let row = &rows[row_idx];

    // Buffer COLUMN, not byte offset — the caret and dot are multi-byte.
    let byte = row.find('●').expect("dot renders");
    let col = row[..byte].chars().count();
    // Map the verdict to its theme hue here, NOT via `health_color`, so a
    // regression in the mapping itself reddens the test instead of moving both
    // sides in lockstep.
    let want = match expected {
        Health::Ok => super::theme::success_color(),
        Health::Warn => super::theme::warning_color(),
        Health::Danger => super::theme::danger_color(),
        Health::Idle => super::theme::text_dim_color(),
    };
    assert_eq!(
        buf.content[row_idx * W as usize + col].fg,
        want,
        "dot hue for {:?}:\n{}",
        expected,
        rows.join("\n")
    );

    // Split at the two adjacent pane borders so the detail pane's own `[f]` line
    // (a different screen row) can't satisfy the selector-marker check.
    let selector = row.split("││").next().unwrap_or(row);
    assert_eq!(
        selector.contains("[f]"),
        expect_fix,
        "selector `[f]` marker for {:?}:\n{}",
        expected,
        rows.join("\n")
    );
}

#[test]
fn herdr_row_renders_ok_dot_without_fix() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(&healthy_probe(), Some(&healthy_config()));
    assert_row(check, Health::Ok, false);
}

#[test]
fn herdr_row_renders_danger_dot_on_registry_warnings() {
    let _home = crate::testutil::HomeSandbox::new();
    let probe = probe(
        Some("0.8.0"),
        Some(entry(true, None, vec!["plugin root is gone"])),
        None,
    );
    let check = herdr_check(&probe, Some(&healthy_config()));
    assert_row(check, Health::Danger, false);
}

#[test]
fn herdr_row_renders_danger_dot_on_registry_error() {
    let _home = crate::testutil::HomeSandbox::new();
    let probe = probe(
        Some("0.8.0"),
        None,
        Some("herdr's plugin list did not parse"),
    );
    let check = herdr_check(&probe, Some(&healthy_config()));
    assert_row(check, Health::Danger, false);
}

#[test]
fn herdr_row_renders_warn_dot_without_fix_when_not_installed() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(&probe(Some("0.8.0"), None, None), Some(&healthy_config()));
    assert_row(check, Health::Warn, false);
}

#[test]
fn herdr_row_renders_warn_dot_without_fix_when_version_too_old() {
    let _home = crate::testutil::HomeSandbox::new();
    let probe = probe(
        Some("0.7.0"),
        Some(entry(true, Some("0.8.0"), vec![])),
        None,
    );
    let check = herdr_check(&probe, Some(&healthy_config()));
    assert_row(check, Health::Warn, false);
}

#[test]
fn herdr_row_renders_warn_dot_without_fix_when_config_does_not_parse() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(
        &healthy_probe(),
        Some(&config(false, None, SidebarState::Absent)),
    );
    assert_row(check, Health::Warn, false);
}

#[test]
fn herdr_row_renders_warn_dot_and_offers_fix_when_key_unbound() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(
        &healthy_probe(),
        Some(&config(true, None, SidebarState::Templated)),
    );
    assert_row(check, Health::Warn, true);
}

#[test]
fn herdr_row_renders_warn_dot_and_offers_fix_when_sidebar_untemplated() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(
        &healthy_probe(),
        Some(&config(true, Some("prefix+a"), SidebarState::Absent)),
    );
    assert_row(check, Health::Warn, true);
}

#[test]
fn herdr_row_renders_warn_dot_without_fix_when_config_unreadable() {
    let _home = crate::testutil::HomeSandbox::new();
    let check = herdr_check(&healthy_probe(), None);
    assert_row(check, Health::Warn, false);
}
