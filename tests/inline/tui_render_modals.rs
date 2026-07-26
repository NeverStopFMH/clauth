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

/// The help modal's GLYPHS legend, rendered whole. It is the only place the
/// account surfaces' 1-cell marks are explained, and two of them carry two
/// meanings apiece split on HUE alone (`⊖` disabled/canceled, `⊘` aggregate/
/// scoped week), so the pin asserts each row's text AND its mark's color — a
/// legend that lost a hue would read as a duplicate entry and sail past a
/// text-only check.
///
/// Driven through `draw_help` rather than `glyph_rows`, so it pins what a user
/// actually sees: the section's placement, its alignment against the key rows,
/// and that nothing clipped it.
#[test]
fn the_help_modal_legend_names_every_marker_and_its_hue() {
    use ratatui::backend::TestBackend;
    use ratatui::{Terminal, style::Color};

    let _tier = crate::testutil::TierSandbox::new(crate::tui::theme::Tier::Full);
    let app = empty_app(Tab::Overview);
    let mut term = Terminal::new(TestBackend::new(100, 60)).unwrap();
    term.draw(|f| draw_help(f, f.area(), &app)).unwrap();
    let buf = term.backend().buffer().clone();
    let rows = crate::testutil::buffer_rows(&buf);
    // Slice each row to the modal's own columns so the pin is the modal alone,
    // not its centering offset within the terminal.
    let (left, right) = rows
        .iter()
        .find_map(|row| {
            let chars: Vec<char> = row.chars().collect();
            Some((
                chars.iter().position(|c| *c == '\u{256d}')?,
                chars.iter().position(|c| *c == '\u{256e}')?,
            ))
        })
        .expect("the help modal's top border");
    let slice =
        |row: &String| -> String { row.chars().skip(left).take(right - left + 1).collect() };

    let head = rows
        .iter()
        .position(|r| r.contains("GLYPHS"))
        .unwrap_or_else(|| panic!("the legend renders:\n{}", rows.join("\n")));
    // The section header, its blank, and one row per mark.
    assert_eq!(
        rows[head..head + 14].iter().map(slice).collect::<Vec<_>>(),
        vec![
            "│  GLYPHS                                                                 │"
                .to_string(),
            "│                                                                         │"
                .to_string(),
            "│    ●                   the active account                               │"
                .to_string(),
            "│    ⇄                   a live session here follows the fallback chain   │"
                .to_string(),
            "│    ⊖                   disabled                                         │"
                .to_string(),
            "│    ⊖                   canceled                                         │"
                .to_string(),
            "│    ×                   auth broken                                      │"
                .to_string(),
            "│    ⊘                   weekly spent                                     │"
                .to_string(),
            "│    ⧗                   claude code blocked                              │"
                .to_string(),
            "│    $                   extra usage spent                                │"
                .to_string(),
            "│    ◔                   5h window spent                                  │"
                .to_string(),
            "│    ⊘                   one model's week spent, other models ok          │"
                .to_string(),
            "│    ~                   past the weekly switch line, still serving       │"
                .to_string(),
            "│    ⋯                   stale data                                       │"
                .to_string(),
        ],
    );

    // Every mark's own hue, read off the rendered cell. The two repeated glyphs
    // are the whole point: same shape, different color, different meaning.
    let expected: [Color; 12] = [
        crate::tui::theme::accent_2_color(),
        crate::tui::theme::text_dim_color(),
        crate::tui::theme::text_faint_color(),
        crate::tui::theme::danger_color(),
        crate::tui::theme::danger_color(),
        crate::tui::theme::danger_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::warning_color(),
        crate::tui::theme::text_faint_color(),
    ];
    // `left + 5`: the modal border, its 2-cell padding, and the row's own
    // 2-space gutter all sit ahead of the mark.
    let glyph_x = left + 5;
    let stride = buf.area.width as usize;
    let got: Vec<Color> = (0..12)
        .map(|i| buf.content[(head + 2 + i) * stride + glyph_x].fg)
        .collect();
    assert_eq!(got, expected.to_vec());
}
