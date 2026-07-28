//! Shell-completions feature coverage: the advertised
//! `clauth completions install [shell]` path. `print_script` is a pure
//! shell→script lookup; `install_rc` / `install_fish` write into home-derived
//! paths, so they run under a home sandbox.

use super::*;

#[test]
fn print_script_supports_bash_zsh_fish() {
    for shell in ["bash", "zsh", "fish"] {
        print_script(shell).unwrap_or_else(|_| panic!("{shell} must be supported"));
    }
}

/// Every shell's script must offer `--isolated` under the `start` subcommand —
/// it's a documented `clauth start` flag (`main.rs`) and was previously uncovered.
#[test]
fn every_shell_completes_start_isolated_flag() {
    // (script body, the flag token as each shell spells its `start` branch)
    let cases = [
        (
            BASH,
            "\"${COMP_WORDS[1]}\" = \"start\" ] && [ \"${cur:0:2}\" = \"--\"",
        ),
        (
            ZSH,
            "\"${words[2]}\" == start ]] && _values 'flag' '--isolated",
        ),
        (FISH, "__fish_seen_subcommand_from start\" -a --isolated"),
    ];
    for (script, branch) in cases {
        assert!(
            script.contains("--isolated"),
            "script must offer --isolated",
        );
        assert!(
            script.contains(branch),
            "the --isolated completion must be gated to the `start` subcommand, not global",
        );
    }
    // Guard against regressing the other subcommands' flags in the same edit.
    assert!(ZSH.contains("--json") && ZSH.contains("--base-url") && ZSH.contains("--force"));
}

/// Every shell's script must offer `--with-fallback` under `start`, gated to
/// that subcommand. The tree walk below scopes each flag to its subcommand's
/// branch; what it does not pin is the exact spelling and ordering inside that
/// branch, which is what these needles hold.
#[test]
fn every_shell_completes_start_with_fallback_flag() {
    let cases = [
        (BASH, "--isolated --rescue --no-rescue --with-fallback"),
        // Anchored on the preceding sibling INSIDE the backslash-continued
        // block. zsh's describe entry is position-free on its own, so a needle
        // made only of it stays green while the line is moved under another
        // subcommand's branch and the flag stops being offered under `start`.
        (
            ZSH,
            "'--no-rescue[isolated only: discard the isolated store]' \\\n            \
             '--with-fallback[follow the fallback chain",
        ),
        (
            FISH,
            "__fish_seen_subcommand_from start\" -a --with-fallback",
        ),
    ];
    for (script, gated) in cases {
        assert!(
            script.contains("--with-fallback"),
            "script must offer --with-fallback",
        );
        assert!(
            script.contains(gated),
            "the --with-fallback completion must be gated to `start`, missing {gated:?}",
        );
    }
}

/// `clauth start --with-fallback <TAB>` is the canonical shape — clap only sees
/// the flag before the profile name — so the profile list has to follow it in the
/// two position-sensitive shells. `--rescue`/`--no-rescue` set no precedent here:
/// both `requires = "isolated"`, so neither is ever the only flag before the name.
/// fish matches on the subcommand alone and is unaffected.
#[test]
fn bash_and_zsh_complete_a_profile_after_start_with_fallback() {
    assert!(
        BASH.contains(
            r#"[ "$prev" = "--isolated" ] || [ "$prev" = "--with-fallback" ] || [ "$prev" = "--profile" ]"#
        ),
        "bash must list profiles after --with-fallback, not only after --isolated",
    );
    assert!(
        ZSH.contains(r#""${words[2]}" == start && "${words[3]}" == (--isolated|--with-fallback)"#),
        "zsh's fourth-word profile arm must accept --with-fallback as the third word",
    );
}

/// Every shell must offer `--setup-token` under the `login` subcommand — the
/// long-lived-token capture flow (#53), gated to login like the other login
/// flags. Mirrors the `--isolated` coverage above.
#[test]
fn every_shell_completes_login_setup_token_flag() {
    let cases = [
        (BASH, "--base-url --api-key --setup-token"),
        (ZSH, "'--setup-token[capture a claude setup-token"),
        (FISH, "__fish_seen_subcommand_from login\" -a --setup-token"),
    ];
    for (script, gated) in cases {
        assert!(
            script.contains("--setup-token"),
            "script must offer --setup-token",
        );
        assert!(
            script.contains(gated),
            "the --setup-token completion must be gated to `login`, missing {gated:?}",
        );
    }
}

/// The scripts are hand-written (clap_complete's stable generator can't
/// reproduce the live `clauth __complete` profile-name shellout), so nothing
/// structural keeps them level with the grammar — they had already drifted three
/// subcommands and a root flag behind it. This walks the real clap `Command`
/// tree and fails on the next drift instead of waiting for someone to notice.
///
/// Each flag is looked up inside its own subcommand's completion branch, not
/// anywhere in the script: spellings repeat across subcommands (`--all` under
/// both `status` and `list`), so a whole-script match would let one of them be
/// deleted while a sibling kept the token alive.
///
/// `help` and `version` are excluded: clap generates them for every command and
/// no shell needs them completed.
#[test]
fn every_visible_subcommand_and_long_flag_is_offered_by_all_three_scripts() {
    use clap::CommandFactory as _;

    let root = crate::cli::Cli::command();
    let generated = ["help", "version"];

    let mut expected: Vec<(String, String)> = root
        .get_arguments()
        .filter(|a| !a.is_hide_set())
        .filter_map(|a| a.get_long())
        .filter(|l| !generated.contains(l))
        .map(|l| ("<root>".to_string(), format!("--{l}")))
        .collect();

    for sub in root.get_subcommands().filter(|s| !s.is_hide_set()) {
        let name = sub.get_name().to_string();
        if generated.contains(&name.as_str()) {
            continue;
        }
        expected.push((name.clone(), name.clone()));
        for long in sub
            .get_arguments()
            .filter(|a| !a.is_hide_set())
            .filter_map(|a| a.get_long())
            .filter(|l| !generated.contains(l))
        {
            expected.push((name.clone(), format!("--{long}")));
        }
    }

    assert!(
        expected.len() > 20,
        "the walk found only {} tokens — it stopped seeing the grammar, \
         so a green run would prove nothing",
        expected.len()
    );
    // The owner half is the dimension the scoping below rests on: a walk that
    // collapsed every pair onto `<root>` would still clear the count guard.
    let owners = expected
        .iter()
        .map(|(owner, _)| owner.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        owners.len() > 10,
        "the walk attributed tokens to only {} owners — it stopped seeing the \
         subcommands, so the per-subcommand scoping below would prove nothing",
        owners.len()
    );

    let mut missing: Vec<String> = Vec::new();
    for (shell, script) in [("bash", BASH), ("zsh", ZSH), ("fish", FISH)] {
        for (owner, token) in &expected {
            // A subcommand's own name is offered by the first-word branch; only
            // its flags live under the branch named after it.
            let branch = if owner == "<root>" || owner == token {
                root_branch(shell, script)
            } else {
                subcommand_branch(shell, script, owner)
            };
            match branch {
                // A bare `contains` would let `--standby` pass on `--no-standby`
                // alone, so match the token with no `-`/alphanumeric neighbour.
                Some(branch) if offers_token(&branch, token) => {}
                Some(_) => missing.push(format!("{shell}: {owner} → {token}")),
                None => missing.push(format!(
                    "{shell}: {owner} → {token} (no `{owner}` branch in the script)"
                )),
            }
        }
    }
    assert!(
        missing.is_empty(),
        "completion scripts have drifted from the clap grammar:\n  {}",
        missing.join("\n  ")
    );
}

/// The slice of `script` that completes the word right after `clauth`: the
/// subcommand names and the root's own flags.
fn root_branch(shell: &str, script: &str) -> Option<String> {
    match shell {
        "bash" => guarded_arms(script, |guard| guard.contains("\"$COMP_CWORD\" -eq 1")),
        "zsh" => guarded_arms(script, |guard| guard.contains("(( CURRENT == 2 ))")),
        "fish" => joined(
            script
                .lines()
                .filter(|l| l.contains("__fish_is_first_token")),
        ),
        _ => None,
    }
}

/// The slice of `script` that completes `name`'s own flags. `None` means the
/// script has no such branch at all — the caller reports that as drift, because
/// falling back to a whole-script search is exactly the hole the scoping closes.
fn subcommand_branch(shell: &str, script: &str, name: &str) -> Option<String> {
    match shell {
        // bash pins the subcommand either as the first word (the flag arms) or
        // as the word just typed (the arms offering a profile after it).
        "bash" => guarded_arms(script, |guard| {
            guard.contains(&format!("\"${{COMP_WORDS[1]}}\" = \"{name}\""))
                || guard.contains(&format!("\"$prev\" = \"{name}\""))
        }),
        // zsh pins it in `[[ "${words[2]}" == … ]]`, bare or as an alternation.
        "zsh" => guarded_arms(script, |guard| {
            guard
                .split("\"${words[2]}\" == ")
                .skip(1)
                .filter_map(|rest| rest.split_whitespace().next())
                .any(|pat| pat.trim_matches(['(', ')']).split('|').any(|a| a == name))
        }),
        // fish pins it in a `__fish_seen_subcommand_from` condition, which may
        // name several subcommands.
        "fish" => joined(script.lines().filter(|l| {
            l.split("__fish_seen_subcommand_from ")
                .skip(1)
                .filter_map(|rest| rest.split('"').next())
                .any(|list| list.split_whitespace().any(|w| w == name))
        })),
        _ => None,
    }
}

/// Every arm of the script's `if`/`elif` chain whose guard line satisfies
/// `owns`, joined. Both the bash and the zsh script are one such chain, and a
/// subcommand can hold more than one arm (zsh spells `resume` in two). An arm
/// is closed at `else`/`fi` as well as opened at `if`/`elif`: without that, the
/// catch-all body and everything trailing the chain inherit the last `elif`'s
/// owner, which is the "right token, wrong place" pass this scoping exists to
/// stop. A closed arm leads with `else`/`fi`, which no `owns` matches, so it is
/// unowned with no extra filtering.
fn guarded_arms(script: &str, owns: impl Fn(&str) -> bool) -> Option<String> {
    let mut arms: Vec<String> = Vec::new();
    let mut arm = String::new();
    for line in script.lines() {
        let head = line.trim_start();
        let opens = head.starts_with("if ") || head.starts_with("elif ");
        let closes = head.starts_with("else") || head == "fi";
        if (opens || closes) && !arm.is_empty() {
            arms.push(std::mem::take(&mut arm));
        }
        arm.push_str(line);
        arm.push('\n');
    }
    arms.push(arm);
    joined(
        arms.iter()
            .filter(|a| owns(a.lines().next().unwrap_or("")))
            .map(String::as_str),
    )
}

fn joined<'a>(parts: impl Iterator<Item = &'a str>) -> Option<String> {
    let text = parts.collect::<Vec<_>>().join("\n");
    (!text.is_empty()).then_some(text)
}

/// An `else` body, and anything trailing the chain, belong to no subcommand.
/// Without closing the arm there they inherit the last `elif`'s owner, so a flag
/// moved into the catch-all still reads as offered under `list`. The real
/// scripts have neither shape, so only a fixture can hold this.
#[test]
fn a_branch_stops_at_the_catch_all_and_at_the_end_of_the_chain() {
    let script = r#"if [ "${COMP_WORDS[1]}" = "status" ]; then
    COMPREPLY=--json
elif [ "${COMP_WORDS[1]}" = "list" ]; then
    COMPREPLY=--all
else
    COMPREPLY=--catch-all
fi
trailing --after-chain
"#;
    let list = subcommand_branch("bash", script, "list").expect("list branch");
    assert!(offers_token(&list, "--all"), "its own arm is in");
    assert!(
        !offers_token(&list, "--catch-all"),
        "the `else` body is nobody's branch",
    );
    assert!(
        !offers_token(&list, "--after-chain"),
        "text past `fi` is nobody's branch",
    );
    assert!(
        !offers_token(&list, "--json"),
        "the sibling arm above stays out",
    );
}

/// The scoping's two load-bearing properties: a branch is a slice of the script
/// and not the whole of it, and an absent branch is `None` rather than a
/// whole-script fallback.
#[test]
fn subcommand_branch_isolates_one_subcommand_or_reports_none() {
    for (shell, script) in [("bash", BASH), ("zsh", ZSH), ("fish", FISH)] {
        let list = subcommand_branch(shell, script, "list")
            .unwrap_or_else(|| panic!("{shell} must have a `list` branch"));
        assert!(offers_token(&list, "--all"), "{shell}: list offers --all");
        assert!(
            !offers_token(&list, "--json"),
            "{shell}: `list` takes no --json, so its branch must not span the \
             sibling branches that do",
        );
        assert!(
            subcommand_branch(shell, script, "nonesuch").is_none(),
            "{shell}: an absent branch must report itself, not fall back",
        );
    }
}

/// Whether `script` offers `token` as a whole word. `--rescue` must not match on
/// `--no-rescue`, nor `start` on `--setup-token`.
fn offers_token(script: &str, token: &str) -> bool {
    let boundary = |c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_');
    script.match_indices(token).any(|(i, _)| {
        let before = script[..i].chars().next_back().is_none_or(boundary);
        let after = script[i + token.len()..]
            .chars()
            .next()
            .is_none_or(boundary);
        before && after
    })
}

#[test]
fn offers_token_does_not_match_inside_a_longer_flag() {
    assert!(offers_token("a --rescue b", "--rescue"));
    assert!(!offers_token("a --no-rescue b", "--rescue"));
    assert!(!offers_token("'--setup-token[x]'", "start"));
    assert!(offers_token("-W \"start login\"", "start"));
}

#[test]
fn print_script_rejects_unsupported_shell() {
    let err = print_script("powershell").expect_err("unsupported shell must error");
    assert!(
        err.to_string().contains("unsupported shell"),
        "error must name the unsupported shell",
    );
}

#[cfg(unix)]
use crate::testutil::HomeSandbox;

/// `completions install bash` writes the script under `~/.clauth/completions/`
/// and appends an idempotent `source` line to `~/.bashrc`.
#[cfg(unix)]
#[test]
fn install_bash_writes_script_and_sources_it_in_rc() {
    let home = HomeSandbox::new();
    let home_path = home.home();

    install(Some("bash")).expect("install bash completions");

    let script = home_path
        .join(".clauth")
        .join("completions")
        .join("clauth.bash");
    assert!(
        script.is_file(),
        "the bash completion script must be written"
    );
    assert!(
        std::fs::read_to_string(&script)
            .expect("read script")
            .contains("complete -F _clauth clauth"),
        "the written script must be the bash completion body",
    );

    let rc = std::fs::read_to_string(home_path.join(".bashrc")).expect("read .bashrc");
    assert!(
        rc.contains(&format!("source \"{}\"", script.display())),
        ".bashrc must source the generated completion script",
    );
}

/// Re-running `install` must not append a second `source` line — the rc edit is
/// idempotent (guarded by the existing-line check).
#[cfg(unix)]
#[test]
fn install_bash_is_idempotent_across_reruns() {
    let home = HomeSandbox::new();
    let home_path = home.home();

    install(Some("bash")).expect("first install");
    install(Some("bash")).expect("second install");

    let rc = std::fs::read_to_string(home_path.join(".bashrc")).expect("read .bashrc");
    let count = rc.matches("# clauth completions").count();
    assert_eq!(count, 1, "the rc source block must be written exactly once");
}

/// Fish does not edit an rc file: the script lands in fish's own completions dir.
#[cfg(unix)]
#[test]
fn install_fish_writes_into_fish_completions_dir() {
    let home = HomeSandbox::new();
    let home_path = home.home();

    install(Some("fish")).expect("install fish completions");

    let script = home_path
        .join(".config")
        .join("fish")
        .join("completions")
        .join("clauth.fish");
    assert!(
        script.is_file(),
        "fish completions must be written to the fish completions dir",
    );
    assert!(
        !home_path.join(".bashrc").exists() && !home_path.join(".zshrc").exists(),
        "installing fish must not touch bash/zsh rc files",
    );
}

#[test]
fn install_rejects_unsupported_shell() {
    let err = install(Some("powershell")).expect_err("unsupported shell must error");
    assert!(
        err.to_string().contains("unsupported shell"),
        "error must name the unsupported shell",
    );
}

// The first-launch consent prompt defaults to Yes: an empty answer (bare Enter)
// installs, so the convenient path stays a single keypress.
#[test]
fn answer_is_yes_defaults_to_yes_on_empty() {
    for a in ["", "   ", "\n", "\r\n"] {
        assert!(answer_is_yes(a), "{a:?} (default) must install");
    }
}

#[test]
fn answer_is_yes_accepts_y_and_yes_any_case() {
    for a in ["y", "Y", "yes", "YES", " Yes "] {
        assert!(answer_is_yes(a), "{a:?} must install");
    }
}

#[test]
fn answer_is_yes_declines_on_n_or_other_input() {
    for a in ["n", "N", "no", "nope", "q", "x"] {
        assert!(!answer_is_yes(a), "{a:?} must decline");
    }
}
