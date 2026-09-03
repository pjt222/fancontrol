//! Guards the standing rule in `CLAUDE.md`: never call
//! `Set_Lighting_Current_Status`.
//!
//! The 82RG's `LENOVO_LIGHTING_METHOD` exposes a getter and a setter. Every
//! measurement so far has used the getter, and what the setter does to the
//! power-button LED, the keyboard backlight, or anything else on this firmware
//! is unmeasured. The rule is stated in prose in `CLAUDE.md` and in the tools'
//! help blocks; this test makes it a build failure, in the same spirit as
//! `shell_portability.rs`: a rule that only lives in a doc is one a future
//! session can miss.
//!
//! What counts as a call: the method name followed by `(`, or passed as a
//! `-Method` argument to `Invoke-LenovoWmiMethod`, on a line that is not a
//! comment. Naming the method in prose (a help block, a `#` comment, a `//`
//! comment, a `.md` file) is allowed, because the docs have to be able to say
//! what not to call.
//!
//! Scanned: every tracked `.ps1`, `.psm1` and `.rs` file, walking the tree the
//! same way `shell_portability.rs` does (no symlinks, no `target`, no `.git`).

use std::fs;
use std::path::{Path, PathBuf};

const FORBIDDEN: &str = "Set_Lighting_Current_Status";

/// Strip PowerShell comments from a script: `<# ... #>` blocks and `#` to end
/// of line. Rust sources get `//` line comments stripped. Quoted strings are
/// not tracked, so a `#` inside a string ends the "code" part of the line
/// early; that can only hide a call, never invent one, and no tool builds a
/// method name inside a string with a `#` in it.
fn code_lines(text: &str, is_rust: bool) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut in_block = false;
    for (idx, raw) in text.lines().enumerate() {
        let mut line = raw.to_string();
        if is_rust {
            if let Some(pos) = line.find("//") {
                line.truncate(pos);
            }
        } else {
            // Block comments may open and close on the same line, or span
            // many. Handle the fragments left over on either side.
            let mut rest = line.clone();
            let mut kept = String::new();
            loop {
                if in_block {
                    match rest.find("#>") {
                        Some(end) => {
                            in_block = false;
                            rest = rest[end + 2..].to_string();
                        }
                        None => {
                            rest.clear();
                            break;
                        }
                    }
                } else {
                    match rest.find("<#") {
                        Some(start) => {
                            kept.push_str(&rest[..start]);
                            in_block = true;
                            rest = rest[start + 2..].to_string();
                        }
                        None => {
                            kept.push_str(&rest);
                            break;
                        }
                    }
                }
            }
            if let Some(pos) = kept.find('#') {
                kept.truncate(pos);
            }
            line = kept;
        }
        if !line.trim().is_empty() {
            out.push((idx + 1, line));
        }
    }
    out
}

/// True when the line invokes the forbidden method rather than merely naming it.
fn invokes_forbidden(code: &str) -> bool {
    let Some(pos) = code.find(FORBIDDEN) else {
        return false;
    };
    let after = code[pos + FORBIDDEN.len()..].trim_start();
    if after.starts_with('(') {
        return true;
    }
    // `-Method 'Set_Lighting_Current_Status'` or `-Method "..."`.
    let before = code[..pos].trim_end();
    let before = before.trim_end_matches(['\'', '"']).trim_end();
    before.ends_with("-Method")
}

fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
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
            Some("ps1") | Some("psm1") | Some("rs")
        ) {
            out.push(path);
        }
    }
}

#[test]
fn no_tracked_source_invokes_the_lighting_setter() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    collect_sources(root, &mut files);
    files.sort();
    assert!(
        files.iter().any(|f| f.ends_with("tools/LenovoWmi.psm1")),
        "scanner found no tools; the walk is broken"
    );

    let mut hits = Vec::new();
    for file in &files {
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        let is_rust = file.extension().and_then(|e| e.to_str()) == Some("rs");
        for (line_no, code) in code_lines(&text, is_rust) {
            // This file names the method in its own code, in the constant.
            if file.ends_with("tests/lighting_setter_forbidden.rs") {
                continue;
            }
            if invokes_forbidden(&code) {
                hits.push(format!(
                    "{}:{}: {}",
                    file.strip_prefix(root).unwrap_or(file).display(),
                    line_no,
                    code.trim()
                ));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "Set_Lighting_Current_Status is never to be called (CLAUDE.md); found:\n{}",
        hits.join("\n")
    );
}

// ---------------------------------------------------------------------------
// The detector, checked against inputs that must and must not trip it
// ---------------------------------------------------------------------------

#[test]
fn a_direct_call_trips_the_detector() {
    assert!(invokes_forbidden(
        "    [void]$lm.Set_Lighting_Current_Status(4, 1, 2)"
    ));
    assert!(invokes_forbidden("$lm.Set_Lighting_Current_Status ($id)"));
}

#[test]
fn a_module_call_trips_the_detector() {
    assert!(invokes_forbidden(
        "Invoke-LenovoWmiMethod -WmiObject $lm -Method 'Set_Lighting_Current_Status' -Property 'x'"
    ));
    assert!(invokes_forbidden(
        "Invoke-LenovoWmiMethod -WmiObject $lm -Method \"Set_Lighting_Current_Status\""
    ));
}

#[test]
fn naming_the_method_in_prose_does_not_trip_it() {
    assert!(!invokes_forbidden(
        "Write-ToolLog \"never calls Set_Lighting_Current_Status here\""
    ));
    assert!(!invokes_forbidden(
        "methods: Get_Lighting_Current_Status, Set_Lighting_Current_Status"
    ));
}

#[test]
fn comments_are_stripped_before_detection() {
    let script = "\
<#
.NOTES
Never calls Set_Lighting_Current_Status(anything).
#>
$x = 1  # Set_Lighting_Current_Status(1) in a trailing comment
<# one-line block Set_Lighting_Current_Status(2) #> $y = 2
$z = 3
";
    let lines = code_lines(script, false);
    let joined: Vec<&str> = lines.iter().map(|(_, l)| l.as_str()).collect();
    assert_eq!(joined, vec!["$x = 1  ", " $y = 2", "$z = 3"]);
    assert!(lines.iter().all(|(_, l)| !invokes_forbidden(l)));
}

#[test]
fn a_call_outside_comments_survives_stripping() {
    let script = "<# help #>\n$r = $lm.Set_Lighting_Current_Status(4)  # comment after\n";
    let lines = code_lines(script, false);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].0, 2);
    assert!(invokes_forbidden(&lines[0].1));
}

#[test]
fn rust_line_comments_are_stripped() {
    let source = "// Set_Lighting_Current_Status(1) in a comment\nlet a = 1; // trailing\n";
    let lines = code_lines(source, true);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].1, "let a = 1; ");
}
