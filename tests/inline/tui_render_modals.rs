use super::*;
use crate::profile::{AppConfig, AppState};
use crate::tui::app::{
    App, ConfigFocus, FallbackFocus, PluginFocus, StatusFocus, TokenView, has_sub_focus,
};

fn empty_app(tab: Tab) -> App {
    let mut app = App::new(AppConfig {
        state: AppState::default(),
        profiles: Vec::new(),
    });
    app.tab = tab;
    app
}

/// Issue #15: a tab with a descend/ascend sub-focus screen (Setup's Actions
/// pane, Fallback's Detail pane, Status/Plugin's Detail pane, Tokens' Models
/// view) must document an `esc` row in its help-modal section, or a user who
/// descended into it has no listed way back.
///
/// Driven off `has_sub_focus` — the same predicate the `q` handler and footer
/// use to decide "back" vs "quit" — rather than a hardcoded tab list, so a
/// future tab wired into that predicate without a matching help row fails
/// here instead of shipping undocumented.
#[test]
fn every_sub_focus_tab_documents_esc_in_help() {
    for tab in Tab::ALL {
        let mut app = empty_app(tab);
        // Drive every sub-focus field to its "descended" value; `has_sub_focus`
        // only reads the one that matches `app.tab`, so this is safe for all.
        app.config_focus = ConfigFocus::Actions;
        app.fallback_focus = FallbackFocus::Detail;
        app.status.focus = StatusFocus::Detail;
        app.plugin.focus = PluginFocus::Detail;
        app.token_view = TokenView::Models;

        if !has_sub_focus(&app) {
            continue;
        }

        let rows = tab_specific_rows(tab);
        let has_esc_row = rows
            .iter()
            .flat_map(|(_, entries)| entries.iter())
            .any(|(key, _)| *key == "esc");
        assert!(
            has_esc_row,
            "tab {tab:?} has a sub-focus but no `esc` row in its help-modal section"
        );
    }
}

/// Pins a tab's `(key, description)` help-modal rows, flattened across
/// sections and in order, exactly — so editing a key, its description, or
/// reordering the rows reds here instead of drifting unnoticed. Flattening
/// drops section titles: every current tab documents exactly one, so nothing
/// is lost. Add another tab's row list to this loop by extending the call.
fn assert_tab_rows(tab: Tab, expected: &[(&str, &str)]) {
    let rows: Vec<(&str, &str)> = tab_specific_rows(tab)
        .iter()
        .flat_map(|(_, entries)| entries.iter().copied())
        .collect();
    assert_eq!(rows, expected, "{tab:?} help-modal row list drifted");
}

#[test]
fn fallback_tab_key_grammar_rows_pin_exact_order_and_copy() {
    assert_tab_rows(
        Tab::Fallback,
        &[
            ("↑↓", "move cursor / detail row"),
            ("shift ↑↓", "reorder to set priority"),
            (
                "↵",
                "open · edit threshold · edit weekly at · edit max spend · toggle gates / last resort · remove · add",
            ),
            ("+ / -", "step rotate at / weekly at by 5"),
            ("↵ on rotate at", "type a value, ↵ saves"),
            ("↵ on weekly at", "type a %, empty clears"),
            ("esc", "back / cancel edit"),
        ],
    );
}
