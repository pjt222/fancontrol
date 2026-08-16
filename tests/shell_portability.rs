//! Guards the portability of shell snippets in tracked documentation.
//!
//! Commands in this repo are authored in a zsh session and executed by CI under
//! bash. A snippet that breaks only under zsh therefore passes every check we
//! run and reaches the user in the README. Worse, the two likeliest breakages
//! are silent: zsh skips an unmatched-glob command while the surrounding list
//! carries on with status 0, and `$PIPESTATUS` reads empty so a documented
//! exit-code check always looks like it passed.
//!
//! This lives as a test rather than a separate CI job so it rides the existing
//! `cargo test` and runs locally, and it is written without `regex` so the
//! dependency tree is unchanged.
//!
//! To exempt a snippet deliberately, put a marker on the line before it:
//! `<!-- portability-exempt: reason -->` in Markdown, `# portability-exempt:
//! reason` in a shell script. Say why — the marker is for snippets that are
//! genuinely shell-specific or illustrative, not for silencing a real finding.

use std::fs;
use std::path::{Path, PathBuf};

/// A portability problem, named by what it does rather than what it matches.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Hazard {
    /// Unquoted word starting with `=`. zsh expands it as a `=command` PATH
    /// lookup, and because that is an expansion failure it aborts the entire
    /// command list — nothing after it runs.
    EqualsWord,
    /// Unquoted glob. If it matches nothing, zsh refuses to run the command
    /// while the surrounding list continues, so the step is silently skipped.
    /// bash passes the literal through instead.
    UnquotedGlob,
    /// `$PIPESTATUS` is a bash-ism. In zsh the array is `$pipestatus` and it is
    /// 1-indexed, so the documented check reads empty and always passes.
    PipeStatus,
    /// `mapfile` / `readarray` are bash builtins with no zsh equivalent.
    BashBuiltin,
}

impl Hazard {
    fn explain(self) -> &'static str {
        match self {
            Hazard::EqualsWord => {
                "unquoted word starting with '=' — zsh reads it as a =command \
                 lookup and aborts the whole command list; quote it"
            }
            Hazard::UnquotedGlob => {
                "unquoted glob — zsh skips the command entirely when it matches \
                 nothing, and the list continues with status 0; quote the pattern"
            }
            Hazard::PipeStatus => {
                "$PIPESTATUS is empty in zsh — the array is $pipestatus and is \
                 1-indexed; prefer running the command bare and reading $?"
            }
            Hazard::BashBuiltin => "mapfile/readarray are bash-only — use a zsh-portable read loop",
        }
    }
}

#[derive(Debug)]
struct Finding {
    file: String,
    line: usize,
    hazard: Hazard,
    text: String,
}

/// Mark each character that sits inside quotes, or is a quote/escape itself.
///
/// A quote character counts as quoted so that a token *opening* with `'` is not
/// mistaken for an unquoted token — `--proto '=https'` must not trip
/// [`Hazard::EqualsWord`], which is exactly the shape the README already uses.
fn quoted_mask(line: &str) -> Vec<bool> {
    let mut mask = Vec::new();
    let (mut in_single, mut in_double, mut escaped) = (false, false, false);

    for ch in line.chars() {
        if escaped {
            mask.push(true);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if !in_single => {
                escaped = true;
                mask.push(true);
            }
            '\'' if !in_double => {
                in_single = !in_single;
                mask.push(true);
            }
            '"' if !in_single => {
                in_double = !in_double;
                mask.push(true);
            }
            _ => mask.push(in_single || in_double),
        }
    }
    mask
}

/// Scan one command line. Comments and blank lines are ignored.
fn scan_line(line: &str) -> Vec<Hazard> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Vec::new();
    }

    let mut found = Vec::new();
    let chars: Vec<char> = line.chars().collect();
    let mask = quoted_mask(line);

    for (i, &ch) in chars.iter().enumerate() {
        if mask[i] {
            continue;
        }
        let starts_token = i == 0 || chars[i - 1].is_whitespace();

        if ch == '=' && starts_token && !found.contains(&Hazard::EqualsWord) {
            found.push(Hazard::EqualsWord);
        }
        if ch == '*' && !found.contains(&Hazard::UnquotedGlob) {
            found.push(Hazard::UnquotedGlob);
        }
    }

    // These are bash-only whatever the quoting, so the mask does not apply.
    if line.contains("PIPESTATUS") {
        found.push(Hazard::PipeStatus);
    }
    if line
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|word| word == "mapfile" || word == "readarray")
    {
        found.push(Hazard::BashBuiltin);
    }

    found
}

fn is_exemption(line: &str) -> bool {
    line.contains("portability-exempt")
}

/// Scan the ```bash / ```sh fences of a Markdown document.
fn scan_markdown(path: &str, text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut in_shell_fence = false;
    let mut fence_exempt = false;
    let mut last_meaningful = String::new();

    for (number, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") {
            if in_shell_fence {
                in_shell_fence = false;
                fence_exempt = false;
            } else {
                let tag = trimmed.trim_start_matches('`').trim();
                in_shell_fence = tag == "bash" || tag == "sh" || tag == "shell" || tag == "zsh";
                // A zsh-tagged fence is zsh by declaration, not by accident.
                fence_exempt = tag == "zsh" || is_exemption(&last_meaningful);
            }
            continue;
        }

        if !line.trim().is_empty() {
            last_meaningful = line.to_string();
        }

        if in_shell_fence && !fence_exempt {
            for hazard in scan_line(line) {
                findings.push(Finding {
                    file: path.to_string(),
                    line: number + 1,
                    hazard,
                    text: line.trim().to_string(),
                });
            }
        }
    }
    findings
}

/// Scan a shell script. Every line is a command line.
fn scan_shell_source(path: &str, text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut previous = String::new();

    for (number, line) in text.lines().enumerate() {
        if !is_exemption(&previous) {
            for hazard in scan_line(line) {
                findings.push(Finding {
                    file: path.to_string(),
                    line: number + 1,
                    hazard,
                    text: line.trim().to_string(),
                });
            }
        }
        previous = line.to_string();
    }
    findings
}

fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();

        // Never follow a symlink. `.claude/agents` and `.claude/skills` are
        // machine-local links into the user's global directory -- hundreds of
        // files that are neither tracked here nor ours to police. Checked with
        // `file_type()` rather than `is_dir()`, which resolves the link.
        if entry.file_type().map(|t| t.is_symlink()).unwrap_or(false) {
            continue;
        }

        if path.is_dir() {
            if matches!(name.as_str(), "target" | ".git" | "node_modules") {
                continue;
            }
            collect_sources(&path, out);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("md") | Some("sh")
        ) {
            out.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// The guard itself
// ---------------------------------------------------------------------------

#[test]
fn documented_shell_snippets_are_portable() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut sources = Vec::new();
    collect_sources(root, &mut sources);

    assert!(
        sources.len() > 3,
        "expected to find the tracked .md files; collected only {:?}. \
         A scan that inspects nothing cannot fail, so this is treated as an error.",
        sources
    );

    let mut findings = Vec::new();
    for path in &sources {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let shown = path
            .strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        if path.extension().and_then(|e| e.to_str()) == Some("sh") {
            findings.extend(scan_shell_source(&shown, &text));
        } else {
            findings.extend(scan_markdown(&shown, &text));
        }
    }

    if !findings.is_empty() {
        let report: Vec<String> = findings
            .iter()
            .map(|f| {
                format!(
                    "  {}:{}\n    {}\n    -> {}",
                    f.file,
                    f.line,
                    f.text,
                    f.hazard.explain()
                )
            })
            .collect();
        panic!(
            "{} shell-portability hazard(s) in documented snippets:\n{}\n\n\
             Fix the snippet, or mark it deliberately with a \
             `portability-exempt: <reason>` comment on the preceding line.",
            findings.len(),
            report.join("\n")
        );
    }
}

// ---------------------------------------------------------------------------
// Positive controls — a scanner that cannot fail proves nothing about the repo
// ---------------------------------------------------------------------------

#[test]
fn scanner_flags_every_hazard_it_claims_to() {
    let fixture = "\
```bash
echo ===
curl --proto =https https://example.com
grep -rn foo --include=*.md .
cmd | tail -5; echo \"EXIT=${PIPESTATUS[0]}\"
mapfile -t arr < file
```";
    let findings = scan_markdown("fixture.md", fixture);

    for expected in [
        Hazard::EqualsWord,
        Hazard::UnquotedGlob,
        Hazard::PipeStatus,
        Hazard::BashBuiltin,
    ] {
        assert!(
            findings.iter().any(|f| f.hazard == expected),
            "scanner missed {expected:?}; found {findings:#?}"
        );
    }
}

#[test]
fn scanner_accepts_the_quoted_forms() {
    // The negative control. If these trip, the guard is unusable and people
    // will exempt their way around it -- the README's rustup line is exactly
    // this shape and must stay clean.
    let fixture = "\
```bash
echo \"===\"
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
grep -rn foo --include=\"*.md\" .
cargo build --release --target x86_64-pc-windows-gnu
```";
    let findings = scan_markdown("fixture.md", fixture);
    assert!(
        findings.is_empty(),
        "false positives on portable snippets: {findings:#?}"
    );
}

#[test]
fn non_shell_fences_are_not_scanned() {
    let fixture = "\
```rust
let glob = \"*\";
let weird = =nonsense;
```";
    assert!(scan_markdown("fixture.md", fixture).is_empty());
}

#[test]
fn exemption_marker_suppresses_a_fence() {
    let hazard = "```bash\necho ===\n```";
    assert!(
        !scan_markdown("f.md", hazard).is_empty(),
        "fixture must be a real hazard, or the exemption test proves nothing"
    );

    let exempted =
        "<!-- portability-exempt: demonstrating the zsh trap -->\n```bash\necho ===\n```";
    assert!(scan_markdown("f.md", exempted).is_empty());
}

#[test]
fn shell_scripts_are_scanned_too() {
    // No .sh files exist in the repo today. This pins the behaviour so one
    // added later is covered rather than silently exempt.
    let script = "#!/usr/bin/env bash\nls *.log\n";
    let findings = scan_shell_source("x.sh", script);
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].hazard, Hazard::UnquotedGlob);

    let exempted = "#!/usr/bin/env bash\n# portability-exempt: bash-only helper\nls *.log\n";
    assert!(scan_shell_source("x.sh", exempted).is_empty());
}

#[test]
fn comments_and_blank_lines_are_ignored() {
    assert!(scan_line("").is_empty());
    assert!(scan_line("   ").is_empty());
    assert!(scan_line("# echo === is the trap, described in prose").is_empty());
}
