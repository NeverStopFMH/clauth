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

use crate::out::{errln, out, outln};

/// The manifest `id`, and the prefix of every qualified action id.
const PLUGIN_ID: &str = "clauth";
/// The action a keybinding points at: opens the dashboard popup.
const OPEN_ACTION: &str = "clauth.open";
/// `owner/repo/subdir`, the only source shape `herdr plugin install` accepts.
const GITHUB_SOURCE: &str = "uwuclxdy/clauth/herdr-plugin";
/// Offered when `--key` is absent. `prefix+` is herdr's own leader.
const DEFAULT_KEY: &str = "prefix+a";
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
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
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

    if !confirm(yes)? {
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
fn herdr_bin() -> String {
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
fn config_path(bin: &str) -> Result<PathBuf> {
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

/// Default-no: this edits a file clauth does not own.
fn confirm(yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !is_tty() {
        errln!("clauth: not a terminal, so herdr's config was left alone; rerun with --yes");
        return Ok(false);
    }
    out!("clauth: write these to herdr's config? [y/N] ");
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

/// Decides what an existing herdr config is missing. Append-only by design: a
/// table the config already defines is reported for the user to merge rather
/// than emitted twice, since a duplicate key is a parse error, and rewriting
/// the file structurally would drop their comments and ordering.
fn plan_config(existing: &str, key: &str) -> Result<ConfigPlan> {
    let doc: toml::Value = toml::from_str(existing)
        .context("herdr's config.toml does not parse; fix it before wiring clauth into it")?;

    let mut plan = ConfigPlan {
        append: String::new(),
        notes: Vec::new(),
    };

    let bound = doc
        .get("keys")
        .and_then(|k| k.get("command"))
        .and_then(toml::Value::as_array)
        .is_some_and(|entries| {
            entries
                .iter()
                .any(|e| e.get("command").and_then(toml::Value::as_str) == Some(OPEN_ACTION))
        });
    if bound {
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

    let rows = doc
        .get("ui")
        .and_then(|u| u.get("sidebar"))
        .and_then(|s| s.get("agents"))
        .and_then(|a| a.get("rows_by_agent"));
    match rows {
        None => {
            plan.try_append(existing, &format!("\n{}", sidebar_block()), "the sidebar row");
        }
        Some(table) => match table.get("claude") {
            Some(row) if mentions_token(row) => {
                plan.notes.push("the sidebar already renders the account, so the rows are left alone".to_string());
            }
            Some(_) => plan.notes.push(format!(
                "your `[ui.sidebar.agents.rows_by_agent]` already sets a claude row, so add `\"{TOKEN}\"` to one of its groups yourself: {SIDEBAR_ROW}"
            )),
            None => plan.notes.push(format!(
                "your `[ui.sidebar.agents.rows_by_agent]` covers other agents, so add this line under it yourself: {SIDEBAR_ROW}"
            )),
        },
    }

    Ok(plan)
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
