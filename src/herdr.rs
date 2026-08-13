//! `clauth herdr install`: the one command that sets the herdr plugin up.
//!
//! herdr owns plugin installation and prints its own preview of every command a
//! plugin would run before registering it, so this shells out with stdio
//! inherited rather than reimplementing that gate or silencing it. The half
//! herdr gives a plugin no way to declare is the reason a setup command exists
//! at all: a keybinding and a sidebar row template both live in the user's own
//! `config.toml`, so both were manual paste steps.
//!
//! Everything written into that config is validated by `herdr config check`
//! against a temporary copy first, which is what catches a `--key` herdr would
//! otherwise disable on load. The real write lands in place rather than through
//! a rename, so the file keeps the mode and inode herdr's config already has.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::out::{errln, out, outln};

/// The manifest `id`, and the prefix of every qualified action id.
const PLUGIN_ID: &str = "clauth";
/// The action a keybinding points at: opens the dashboard popup.
const OPEN_ACTION: &str = "clauth.open";
/// `owner/repo/subdir`, the only source shape `herdr plugin install` accepts.
const GITHUB_SOURCE: &str = "uwuclxdy/clauth/herdr-plugin";
/// Offered when `--key` is absent. `prefix+` is herdr's own leader.
pub(crate) const DEFAULT_KEY: &str = "prefix+a";
/// The pane-metadata name `report-profile.sh` publishes the account under.
const TOKEN: &str = "$clauth";
/// Marks this crate's additions inside a file clauth does not own.
const MARKER: &str = "# clauth herdr plugin";
/// The agent row that renders the token. Claude Code panes take the
/// `rows_by_agent` template rather than the generic `rows`.
const SIDEBAR_ROW: &str = r#"claude = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$clauth"]]"#;

/// Where the plugin comes from. A checkout gets linked in place so an edit is
/// live on the next open; anyone else fetches the published subdir.
enum Source {
    Link(PathBuf),
    Github,
}

pub(crate) fn install(key: Option<&str>, no_config: bool, yes: bool) -> Result<()> {
    // `herdr-plugin/herdr-plugin.toml` declares linux and macos, because its
    // entrypoints are POSIX shell. herdr links a plugin whose platforms exclude
    // the host and refuses each entrypoint at invocation instead, so without
    // this the command lands a plugin that answers `platform_unsupported` to
    // every key the same command just bound.
    if cfg!(windows) {
        bail!("the herdr plugin is linux and macos only: its entrypoints are POSIX shell scripts");
    }

    let bin = herdr_bin();

    match plugin_source() {
        Source::Link(path) => {
            outln!("clauth: linking {} into herdr", path.display());
            let path = path.to_string_lossy().into_owned();
            // `plugin link` answers with the whole parsed manifest as one JSON
            // line and asks nothing, so it is swallowed unless it fails.
            run_quiet(&bin, &["plugin", "link", &path])?;
        }
        Source::Github => {
            outln!("clauth: installing {GITHUB_SOURCE} into herdr");
            let mut args = vec!["plugin", "install", GITHUB_SOURCE];
            // herdr's preview is the user's chance to read what a plugin will
            // run as them. Only skip it when this command was already answered.
            if yes {
                args.push("--yes");
            }
            run(&bin, &args)?;
        }
    }

    // Ahead of the --no-config branch: that path prints a block to paste, and
    // a key that breaks the file breaks it just as thoroughly by hand.
    let key = resolve_key(key, yes)?;

    if no_config {
        outln!("clauth: herdr's config left alone (--no-config)");
        print_manual(&key);
        return Ok(());
    }

    let path = config_path(&bin)?;
    let existing = read_config(&path)?;
    let plan = plan_config(&existing, &key)?;

    for note in &plan.notes {
        outln!("clauth: {note}");
    }

    if plan.append.is_empty() {
        outln!("clauth: herdr's config already carries everything clauth would add");
        return Ok(());
    }

    outln!("");
    outln!("{}:", path.display());
    for line in plan.append.trim_start_matches('\n').lines() {
        outln!("+ {line}");
    }
    outln!("");

    if !confirm("write these to herdr's config?", yes)? {
        outln!("clauth: nothing written");
        print_manual(&key);
        return Ok(());
    }

    let text = with_append(&existing, &plan.append);
    write_validated(&path, &existing, &text, &bin)?;

    outln!("clauth: wrote {}", path.display());
    outln!("clauth: press {key} in herdr to open the dashboard");
    Ok(())
}

/// The running herdr when clauth was launched from one of its panes, else
/// whatever is on `PATH`. Inside a pane the injected path names the binary that
/// owns the session being configured, which a bare name can miss.
pub(crate) fn herdr_bin() -> String {
    std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".to_string())
}

/// A checkout is recognized by the manifest rather than by a repo name, so a
/// fork or a rename still links.
fn plugin_source() -> Source {
    let dir = std::env::current_dir()
        .unwrap_or_default()
        .join("herdr-plugin");
    if dir.join("herdr-plugin.toml").is_file() {
        return Source::Link(dir);
    }
    Source::Github
}

fn run(bin: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(bin).args(args).status().with_context(|| {
        format!(
            "could not run `{bin} {}`; is herdr installed and on PATH?",
            args.join(" ")
        )
    })?;
    if !status.success() {
        bail!("`{bin} {}` failed", args.join(" "));
    }
    Ok(())
}

/// Same, for a command that neither prompts nor prints anything a user wants.
/// Its output still reaches them when it fails, which is the only time it says
/// something they can act on.
fn run_quiet(bin: &str, args: &[&str]) -> Result<()> {
    let out = Command::new(bin).args(args).output().with_context(|| {
        format!(
            "could not run `{bin} {}`; is herdr installed and on PATH?",
            args.join(" ")
        )
    })?;
    if !out.status.success() {
        let mut why = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let err = String::from_utf8_lossy(&out.stderr);
        if !err.trim().is_empty() {
            why.push('\n');
            why.push_str(err.trim());
        }
        bail!("`{bin} {}` failed:\n{why}", args.join(" "));
    }
    Ok(())
}

/// herdr resolves its own config root per OS and exposes no command that prints
/// it, so this derives the root from the one path command it does have: a
/// plugin config dir is always `<root>/plugins/config/<component>`.
pub(crate) fn config_path(bin: &str) -> Result<PathBuf> {
    if let Ok(explicit) = std::env::var("HERDR_CONFIG_PATH")
        && !explicit.is_empty()
    {
        return Ok(PathBuf::from(explicit));
    }

    let out = Command::new(bin)
        .args(["plugin", "config-dir", PLUGIN_ID])
        .output()
        .with_context(|| format!("could not run `{bin} plugin config-dir`"))?;
    let printed = String::from_utf8_lossy(&out.stdout);
    // Guessing a second location is worse than failing here: a guess that
    // misses writes a config file herdr never reads, and the user is told they
    // are set up. `dirs::config_dir()` is exactly that guess on macOS.
    config_path_from_plugin_dir(printed.trim()).with_context(|| {
        format!(
            "could not work out where herdr keeps its config (`{bin} plugin config-dir` printed \
             {printed:?}); pass the file yourself with HERDR_CONFIG_PATH"
        )
    })
}

/// herdr prints `<root>/plugins/config/<component>`, and `<root>` is where its
/// `config.toml` lives. Three components off the end, and the result has to
/// still be a real prefix rather than the empty path a relative print leaves.
fn config_path_from_plugin_dir(printed: &str) -> Option<PathBuf> {
    if printed.is_empty() {
        return None;
    }
    let root = PathBuf::from(printed).ancestors().nth(3)?.to_path_buf();
    // The empty path comes off a relative print, and a root with no parent of
    // its own is the filesystem root: neither is a directory herdr keeps a
    // config in, and both would have this write somewhere nobody reads.
    if root.as_os_str().is_empty() || root.parent().is_none() {
        return None;
    }
    Some(root.join("config.toml"))
}

/// Reads herdr's config for the callers that edit it. A missing file is an absent config and reads as empty; any other failure is a real error, since writing an empty string back would destroy a config that merely failed to read.
fn read_config(path: &Path) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| {
            format!(
                "cannot read herdr's config at {} (encoding or permissions); fix it before clauth edits it",
                path.display()
            )
        }),
    }
}

/// One clauth entry from `herdr plugin list --json`. Every field is optional: herdr's schema is read leniently, so a shape change degrades to "unknown" rather than an error, the same way the Plugin tab reads CC's registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RegistryEntry {
    pub(crate) enabled: bool,
    pub(crate) version: Option<String>,
    pub(crate) min_herdr_version: Option<String>,
    pub(crate) plugin_root: Option<String>,
    pub(crate) source_kind: Option<String>,
    pub(crate) warnings: Vec<String>,
}

/// Everything the Plugin tab's herdr row needs that costs a subprocess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HerdrProbe {
    /// The version token after `herdr ` in `herdr --version`.
    pub(crate) version: Option<String>,
    /// `None` when clauth is not in the registry.
    pub(crate) entry: Option<RegistryEntry>,
    pub(crate) config_path: Option<PathBuf>,
    /// Registry read failed (not "absent"); `None` when it was only absent.
    pub(crate) error: Option<String>,
}

/// Probes the installed herdr. `None` when herdr does not resolve, so the caller renders no row at all.
#[allow(
    dead_code,
    reason = "consumed by the Plugin tab's herdr row (T24 lane B)"
)]
pub(crate) fn probe() -> Option<HerdrProbe> {
    let bin = herdr_bin();
    let bin = if bin == "herdr" {
        crate::plugin_probe::on_path("herdr")?
            .to_string_lossy()
            .into_owned()
    } else {
        bin
    };

    let version = version_command(&bin);
    let (entry, error) = registry_probe(&bin);
    let config_path = config_path(&bin).ok();

    Some(HerdrProbe {
        version,
        entry,
        config_path,
        error,
    })
}

fn version_command(bin: &str) -> Option<String> {
    let out = Command::new(bin).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    version_from(text.lines().next()?)
}

/// `herdr 0.8.0` -> `Some("0.8.0")`. Pure, so the test feeds the real line.
fn version_from(line: &str) -> Option<String> {
    line.trim()
        .strip_prefix("herdr ")?
        .split_whitespace()
        .next()
        .map(str::to_string)
}

fn registry_probe(bin: &str) -> (Option<RegistryEntry>, Option<String>) {
    let out = match Command::new(bin)
        .args(["plugin", "list", "--json"])
        .output()
    {
        Ok(out) => out,
        Err(e) => {
            return (
                None,
                Some(format!("could not run `{bin} plugin list --json`: {e}")),
            );
        }
    };
    if !out.status.success() {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.trim();
        return (
            None,
            Some(if why.is_empty() {
                format!("`{bin} plugin list --json` failed")
            } else {
                format!("`{bin} plugin list --json` failed: {why}")
            }),
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let root: Value = match serde_json::from_str(&text) {
        Ok(root) => root,
        Err(e) => {
            return (
                None,
                Some(format!("herdr's plugin list did not parse: {e}")),
            );
        }
    };
    (registry_entry_from_value(&root), None)
}

/// The pure half of the registry read, split out so tests feed it the real bytes with no subprocess.
#[cfg(test)]
fn registry_entry_from(json: &str) -> Option<RegistryEntry> {
    let root: Value = serde_json::from_str(json).ok()?;
    registry_entry_from_value(&root)
}

fn registry_entry_from_value(root: &Value) -> Option<RegistryEntry> {
    let entry = root
        .get("result")?
        .get("plugins")?
        .as_array()?
        .iter()
        .find(|e| e.get("plugin_id").and_then(Value::as_str) == Some(PLUGIN_ID))?;

    let field = |key: &str| entry.get(key).and_then(Value::as_str).map(str::to_string);
    Some(RegistryEntry {
        // A listed plugin is enabled unless herdr says otherwise, so an absent `enabled` reads as enabled rather than disabled.
        enabled: entry
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        version: field("version"),
        min_herdr_version: field("min_herdr_version"),
        plugin_root: field("plugin_root"),
        source_kind: entry
            .get("source")
            .and_then(|source| source.get("kind"))
            .and_then(Value::as_str)
            .map(str::to_string),
        warnings: entry
            .get("warnings")
            .and_then(Value::as_array)
            .map(|warnings| {
                warnings
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

fn resolve_key(key: Option<&str>, yes: bool) -> Result<String> {
    let key = match key {
        Some(k) => k.trim().to_string(),
        None if yes || !is_tty() => DEFAULT_KEY.to_string(),
        None => {
            out!("clauth: key that opens the dashboard [{DEFAULT_KEY}] ");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            match line.trim() {
                "" => DEFAULT_KEY.to_string(),
                answer => answer.to_string(),
            }
        }
    };
    validate_key(&key)?;
    Ok(key)
}

/// Bounds only what could break the file. herdr is the authority on whether a
/// spec means anything, and `config check` reports the ones it would disable
/// before any of this reaches the real config.
fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 64 {
        bail!("key '{key}' is empty or too long; expected a herdr spec like `{DEFAULT_KEY}`");
    }
    if key.chars().any(|c| c == '"' || c == '\\' || c.is_control()) {
        bail!("key '{key}' carries a quote, a backslash, or a control character");
    }
    Ok(())
}

fn is_tty() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// Default-no: every caller changes something clauth does not own. The question is the caller's, since one of them adds to a config and the other removes a plugin as well as config lines.
fn confirm(question: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !is_tty() {
        errln!("clauth: not a terminal, so nothing was changed; rerun with --yes");
        return Ok(false);
    }
    out!("clauth: {question} [y/N] ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let answer = line.trim();
    Ok(answer.eq_ignore_ascii_case("y") || answer.eq_ignore_ascii_case("yes"))
}

fn print_manual(key: &str) {
    outln!("clauth: add these to herdr's config.toml yourself:");
    outln!("");
    outln!("{}", binding_block(key));
    outln!("{}", sidebar_block());
}

fn binding_block(key: &str) -> String {
    format!(
        "{MARKER}\n[[keys.command]]\nkey = \"{key}\"\ntype = \"plugin_action\"\ncommand = \"{OPEN_ACTION}\"\ndescription = \"clauth accounts\"\n"
    )
}

fn sidebar_block() -> String {
    format!(
        "{MARKER}: `{TOKEN}` renders the account each Claude Code pane burns\n[ui.sidebar.agents.rows_by_agent]\n{SIDEBAR_ROW}\n"
    )
}

/// What `plan_config` decided: text to append, plus what it refused to touch.
struct ConfigPlan {
    append: String,
    notes: Vec<String>,
}

impl ConfigPlan {
    /// Takes a block only when the file it lands in still parses with it.
    ///
    /// Walking the parsed tree answers "is this defined", never "can a header
    /// for it be appended": `ui = { sidebar = ... }` reads as an absent
    /// `ui.sidebar.agents.rows_by_agent` and rejects the header that would
    /// extend it, and a plain `[keys.command]` table rejects an appended
    /// `[[keys.command]]`. Both are valid TOML nobody would call unusual, so
    /// the block is tried against the real text and handed over on a miss.
    fn try_append(&mut self, existing: &str, block: &str, what: &str) {
        let candidate = with_append(existing, &format!("{}{block}", self.append));
        if toml::from_str::<toml::Value>(&candidate).is_ok() {
            self.append.push_str(block);
            return;
        }
        self.notes.push(format!(
            "your config spells the table {what} belongs in a way clauth cannot extend by appending, so add it yourself:\n{}",
            block.trim_start_matches('\n')
        ));
    }
}

/// The one place the two halves are glued, so a test that pins the seam is
/// pinning what `install` runs rather than a second copy of it.
fn with_append(existing: &str, append: &str) -> String {
    let mut text = existing.to_string();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(append);
    text
}

/// What the sidebar half of the config says. Maps one-to-one onto the four arms `plan_config` matches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebarState {
    /// The claude row already renders the token.
    Templated,
    /// The claude row exists but does not render the token.
    OtherClaudeRow,
    /// `rows_by_agent` covers other agents but has no claude row.
    OtherAgentsOnly,
    /// No `rows_by_agent` table at all.
    Absent,
}

/// The config-side verdicts the Plugin tab's herdr row shows, read straight from the parsed document. `parsed` is false when the file does not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfigStatus {
    pub(crate) parsed: bool,
    /// The key spelling bound to `clauth.open`, when one is.
    pub(crate) bound_key: Option<String>,
    pub(crate) sidebar: SidebarState,
}

/// Pure string -> verdict. The caller does the file read, so the row can show a missing or unreadable file without a second parse.
#[allow(
    dead_code,
    reason = "consumed by the Plugin tab's herdr row (T24 lane B)"
)]
pub(crate) fn config_status(existing: &str) -> ConfigStatus {
    match toml::from_str::<toml::Value>(existing) {
        Ok(doc) => ConfigStatus {
            parsed: true,
            bound_key: bound_key(&doc),
            sidebar: sidebar_state(&doc),
        },
        Err(_) => ConfigStatus {
            parsed: false,
            bound_key: None,
            sidebar: SidebarState::Absent,
        },
    }
}

/// The key spelling of the entry bound to `clauth.open`, if any.
fn bound_key(doc: &toml::Value) -> Option<String> {
    doc.get("keys")
        .and_then(|k| k.get("command"))
        .and_then(toml::Value::as_array)
        .and_then(|entries| {
            entries
                .iter()
                .find(|e| e.get("command").and_then(toml::Value::as_str) == Some(OPEN_ACTION))
        })
        .and_then(|e| e.get("key").and_then(toml::Value::as_str))
        .map(str::to_string)
}

fn sidebar_state(doc: &toml::Value) -> SidebarState {
    let Some(table) = doc
        .get("ui")
        .and_then(|u| u.get("sidebar"))
        .and_then(|s| s.get("agents"))
        .and_then(|a| a.get("rows_by_agent"))
    else {
        return SidebarState::Absent;
    };
    match table.get("claude") {
        Some(row) if mentions_token(row) => SidebarState::Templated,
        Some(_) => SidebarState::OtherClaudeRow,
        None => SidebarState::OtherAgentsOnly,
    }
}

/// Decides what an existing herdr config is missing. Append-only by design: a
/// table the config already defines is reported for the user to merge rather
/// than emitted twice, since a duplicate key is a parse error, and rewriting
/// the file structurally would drop their comments and ordering.
///
/// Both verdicts route through the same helpers `config_status` uses, so the row and the install plan cannot drift.
fn plan_config(existing: &str, key: &str) -> Result<ConfigPlan> {
    let doc: toml::Value = toml::from_str(existing)
        .context("herdr's config.toml does not parse; fix it before wiring clauth into it")?;

    let mut plan = ConfigPlan {
        append: String::new(),
        notes: Vec::new(),
    };

    if bound_key(&doc).is_some() {
        plan.notes.push(format!(
            "`{OPEN_ACTION}` is already bound, so the keybinding is left alone"
        ));
    } else {
        plan.try_append(
            existing,
            &format!("\n{}", binding_block(key)),
            "the keybinding",
        );
    }

    match sidebar_state(&doc) {
        SidebarState::Templated => plan.notes.push(
            "the sidebar already renders the account, so the rows are left alone".to_string(),
        ),
        SidebarState::OtherClaudeRow => plan.notes.push(format!(
            "your `[ui.sidebar.agents.rows_by_agent]` already sets a claude row, so add `\"{TOKEN}\"` to one of its groups yourself: {SIDEBAR_ROW}"
        )),
        SidebarState::OtherAgentsOnly => plan.notes.push(format!(
            "your `[ui.sidebar.agents.rows_by_agent]` covers other agents, so add this line under it yourself: {SIDEBAR_ROW}"
        )),
        SidebarState::Absent => {
            plan.try_append(existing, &format!("\n{}", sidebar_block()), "the sidebar row");
        }
    }

    Ok(plan)
}

/// Appends whatever `plan_config` says is missing. Returns the plan's notes (the pieces it refused to touch), empty when it wrote everything.
#[allow(
    dead_code,
    reason = "consumed by the Plugin tab's herdr row (T24 lane B)"
)]
pub(crate) fn heal(config_path: &Path, key: &str, bin: &str) -> Result<Vec<String>> {
    let existing = read_config(config_path)?;
    let plan = plan_config(&existing, key)?;
    if !plan.append.is_empty() {
        let text = with_append(&existing, &plan.append);
        write_validated(config_path, &existing, &text, bin)?;
    }
    Ok(plan.notes)
}

/// Test seam over [`strip_marked_blocks`], so the round-trip tests name the rule they pin.
#[cfg(test)]
fn without_marked_blocks(existing: &str) -> String {
    strip_marked_blocks(existing).0
}

/// Drops every block this crate marked with `MARKER`, nothing else, plus the lines it removed in order so `uninstall` can print a `- ` diff that mirrors the `+ ` one `install` prints.
///
/// A block is real only when a `[`-leading line follows the marker, since that header is what `install` always writes; the blank `install` prepends before it is dropped too. A marker standing alone drops itself and leaves the next line on the normal path. The residue is a marker inside a multi-line string whose next line happens to begin with `[`; telling that apart needs a TOML parser, and this strip only runs over lines `install` wrote, where the header always follows.
fn strip_marked_blocks(existing: &str) -> (String, Vec<String>) {
    let mut out: Vec<String> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    let mut skipping = false;
    let mut lines = existing.split_inclusive('\n').peekable();

    while let Some(raw) = lines.next() {
        let content = raw.strip_suffix('\n').unwrap_or(raw);
        let lead = content.trim_start();

        if skipping {
            if lead.is_empty() || lead.starts_with('[') {
                skipping = false;
                out.push(raw.to_string());
            } else {
                removed.push(content.to_string());
            }
            continue;
        }

        if lead.starts_with(MARKER) {
            // Pop the blank install prepends only when a real block follows, so
            // a standalone marker keeps the line above it too.
            if lines
                .peek()
                .is_some_and(|next| next.trim_start().starts_with('['))
                && out.last().is_some_and(|last| last.trim().is_empty())
                && let Some(blank) = out.pop()
            {
                removed.push(blank.strip_suffix('\n').unwrap_or(&blank).to_string());
            }
            removed.push(content.to_string());
            // `next_if` leaves a non-`[` line for the normal path rather than
            // consuming it as a header.
            if let Some(header) = lines.next_if(|next| next.trim_start().starts_with('[')) {
                removed.push(header.strip_suffix('\n').unwrap_or(header).to_string());
                skipping = true;
            }
            continue;
        }

        out.push(raw.to_string());
    }

    (out.concat(), removed)
}

pub(crate) fn uninstall(no_config: bool, yes: bool) -> Result<()> {
    let bin = herdr_bin();

    // Read and strip before touching herdr, so one confirm covers both halves
    // and a decline leaves the plugin and the config both untouched.
    let config_edit: Option<(PathBuf, String, String, Vec<String>)> = if no_config {
        None
    } else {
        let path = config_path(&bin)?;
        let previous = read_config(&path)?;
        let (text, removed) = strip_marked_blocks(&previous);
        (text != previous).then_some((path, previous, text, removed))
    };

    outln!("clauth: this removes the clauth plugin from herdr");
    if let Some((path, _, _, removed)) = &config_edit {
        let mut diff = removed.clone();
        // The first removed line is the blank `install` prepends before its first block; `install`'s diff trims it, so this one does too.
        if diff.first().is_some_and(String::is_empty) {
            diff.remove(0);
        }
        outln!("");
        outln!("{}:", path.display());
        for line in &diff {
            outln!("- {line}");
        }
        outln!("");
    }

    let question = if config_edit.is_some() {
        "remove the plugin and these config lines?"
    } else {
        "remove the clauth plugin from herdr?"
    };
    if !confirm(question, yes)? {
        outln!("clauth: nothing changed");
        return Ok(());
    }

    match uninstall_plugin(&bin)? {
        PluginUninstall::Done => outln!("clauth: uninstalled the herdr plugin"),
        PluginUninstall::NotInstalled => {
            outln!("clauth: herdr had no clauth plugin to uninstall (plugin not installed)")
        }
    }

    if let Some((path, previous, text, _)) = config_edit {
        write_validated(&path, &previous, &text, &bin)?;
        outln!("clauth: removed clauth's additions from {}", path.display());
    }

    Ok(())
}

enum PluginUninstall {
    Done,
    NotInstalled,
}

/// `herdr plugin uninstall clauth`. herdr exits 1 with a `plugin not installed` line when there is nothing to remove; the caller treats that as a no-op. The phrase must start a line, so a real failure that merely mentions it still fails.
fn uninstall_plugin(bin: &str) -> Result<PluginUninstall> {
    let out = Command::new(bin)
        .args(["plugin", "uninstall", PLUGIN_ID])
        .output()
        .with_context(|| {
            format!(
                "could not run `{bin} plugin uninstall {PLUGIN_ID}`; is herdr installed and on PATH?"
            )
        })?;
    if out.status.success() {
        return Ok(PluginUninstall::Done);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let not_installed = format!("{stdout}\n{stderr}")
        .lines()
        .any(|line| line.trim_start().starts_with("plugin not installed"));
    if out.status.code() == Some(1) && not_installed {
        return Ok(PluginUninstall::NotInstalled);
    }
    let mut why = stdout.trim().to_string();
    let err = stderr.trim();
    if !err.is_empty() {
        why.push('\n');
        why.push_str(err);
    }
    bail!("`{bin} plugin uninstall {PLUGIN_ID}` failed:\n{why}");
}

/// Walks a row template looking for the token. A row is an array of arrays of
/// strings, so a plain string search over a rendering would depend on how the
/// toml crate happens to format one.
fn mentions_token(value: &toml::Value) -> bool {
    match value {
        toml::Value::String(s) => s == TOKEN,
        toml::Value::Array(items) => items.iter().any(mentions_token),
        _ => false,
    }
}

/// Writes only what herdr accepts. The check runs against a copy, so a rejected
/// edit never reaches the real file, and the real write lands in place, so the
/// file keeps its own mode.
///
/// A config already carrying a complaint of its own still gets wired. `herdr
/// config check` diagnoses the whole file, so refusing on its exit code alone
/// locks anyone with one stale key out of this command over something that
/// predates it. Only a diagnostic this edit ADDS is clauth's to refuse over.
fn write_validated(path: &Path, previous: &str, text: &str, bin: &str) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let probe = tempfile::Builder::new()
        .prefix(".clauth-herdr")
        .tempfile_in(dir)?;

    let before = check_config(bin, probe.path(), previous)?;
    let after = check_config(bin, probe.path(), text)?;
    let added = added_diagnostics(&before, &after);
    if !added.is_empty() {
        bail!(
            "herdr rejected what clauth would add, so nothing changed:\n{}",
            added.join("\n")
        );
    }
    for stale in &before {
        errln!("clauth: herdr already says this about your config: {stale}");
    }

    // Shortcut, with its ceiling: a truncating in-place write is what keeps the
    // file's mode and inode, and its cost is that a crash or a full disk mid-
    // write leaves the config short. The upgrade is write-temp-then-rename with
    // the original's mode read and restored onto the temp first, which is worth
    // doing the day this writes anything a user cannot retype.
    std::fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

/// Diagnostics `after` carries that `before` did not, which is the only set
/// this command answers for. Order-preserving, and a line repeated in `after`
/// counts once, matching how the message reads.
fn added_diagnostics<'a>(before: &[String], after: &'a [String]) -> Vec<&'a str> {
    let mut added: Vec<&str> = Vec::new();
    for line in after {
        if !before.contains(line) && !added.contains(&line.as_str()) {
            added.push(line);
        }
    }
    added
}

/// `herdr config check` over `text`, as its diagnostic lines. An accepted
/// config answers with none, so callers compare two runs rather than two exit
/// codes.
fn check_config(bin: &str, probe: &Path, text: &str) -> Result<Vec<String>> {
    std::fs::write(probe, text)?;
    let out = Command::new(bin)
        .args(["config", "check"])
        .env("HERDR_CONFIG_PATH", probe)
        .output()
        .with_context(|| format!("could not run `{bin} config check`"))?;
    if out.status.success() {
        return Ok(Vec::new());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .chain(String::from_utf8_lossy(&out.stderr).lines())
        .map(str::trim)
        // The header names the outcome rather than an issue, and it reads the
        // same on both runs, so it would never survive the diff anyway.
        .filter(|line| !line.is_empty() && *line != "config: issues found")
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
#[path = "../tests/inline/herdr.rs"]
mod tests;
