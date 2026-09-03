//! Guards two standing rules as build failures.
//!
//! 1. `CLAUDE.md`: never call `Set_Lighting_Current_Status`. The 82RG's
//!    `LENOVO_LIGHTING_METHOD` exposes a getter and a setter. Every measurement
//!    so far has used the getter, and what the setter does to the power-button
//!    LED, the keyboard backlight, or anything else on this firmware is
//!    unmeasured. Scanned: every tracked `.ps1`, `.psm1` and `.rs` file.
//! 2. `tools/Watch-LenovoLightingVsMode.ps1` never calls `SetSmartFanMode`.
//!    That is the tool's headline property: it samples the mode while the
//!    operator moves it by other paths, and a "put the mode back" convenience
//!    added later would silently turn it into the sweep it exists to differ
//!    from.
//!
//! Both rules were prose (in `CLAUDE.md` and the tools' help blocks); this
//! test makes them fail `cargo test`, in the same spirit as
//! `shell_portability.rs`: a rule that only lives in a doc is one a future
//! session can miss.
//!
//! What counts as a call: the method name followed by `(` or `.Invoke`, or
//! passed to `-Method` / `-MethodName`, on a line that is not a comment, at
//! any occurrence on the line. Naming the method in prose (a help block, a
//! `#` comment, a `//` comment, a log string, a `.md` file) is allowed,
//! because the docs have to be able to say what not to call.
//!
//! The walk mirrors `shell_portability.rs`: no symlinks, no `target`, no
//! `.git`.

use std::fs;
use std::path::{Path, PathBuf};

const LIGHTING_SETTER: &str = "Set_Lighting_Current_Status";
const MODE_SETTER: &str = "SetSmartFanMode";
const WATCH_TOOL: &str = "tools/Watch-LenovoLightingVsMode.ps1";

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

/// True when the line invokes `name` rather than merely naming it. Every
/// occurrence on the line is checked, so a prose mention before a real call
/// does not mask the call.
fn invokes(code: &str, name: &str) -> bool {
    code.match_indices(name).any(|(pos, _)| {
        let after = code[pos + name.len()..].trim_start();
        if after.starts_with('(') || after.starts_with(".Invoke") {
            return true;
        }
        // `-Method 'Name'`, `-Method "Name"`, `-MethodName 'Name'`.
        let before = code[..pos].trim_end();
        let before = before.trim_end_matches(['\'', '"']).trim_end();
        before.ends_with("-Method") || before.ends_with("-MethodName")
    })
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

fn hits_in(files: &[PathBuf], root: &Path, name: &str) -> Vec<String> {
    let mut hits = Vec::new();
    for file in files {
        // This file names both methods in its own code, in the constants.
        if file.ends_with("tests/lighting_setter_forbidden.rs") {
            continue;
        }
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        let is_rust = file.extension().and_then(|e| e.to_str()) == Some("rs");
        for (line_no, code) in code_lines(&text, is_rust) {
            if invokes(&code, name) {
                hits.push(format!(
                    "{}:{}: {}",
                    file.strip_prefix(root).unwrap_or(file).display(),
                    line_no,
                    code.trim()
                ));
            }
        }
    }
    hits
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
    let hits = hits_in(&files, root, LIGHTING_SETTER);
    assert!(
        hits.is_empty(),
        "Set_Lighting_Current_Status is never to be called (CLAUDE.md); found:\n{}",
        hits.join("\n")
    );
}

#[test]
fn the_watch_tool_never_invokes_setsmartfanmode() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let tool = root.join(WATCH_TOOL);
    assert!(tool.is_file(), "{} is missing", WATCH_TOOL);
    let files = vec![tool];
    let hits = hits_in(&files, root, MODE_SETTER);
    assert!(
        hits.is_empty(),
        "{} must never call SetSmartFanMode; found:\n{}",
        WATCH_TOOL,
        hits.join("\n")
    );
    // Positive control: the sweep tool does call it, so the detector can see
    // a real call in a real file of this repo.
    let sweep = vec![root.join("tools/Get-LenovoLighting.ps1")];
    assert!(
        !hits_in(&sweep, root, MODE_SETTER).is_empty(),
        "positive control failed: the detector does not see Get-LenovoLighting.ps1's SetSmartFanMode call"
    );
}

// ---------------------------------------------------------------------------
// The detector, checked against inputs that must and must not trip it
// ---------------------------------------------------------------------------

#[test]
fn a_direct_call_trips_the_detector() {
    assert!(invokes(
        "    [void]$lm.Set_Lighting_Current_Status(4, 1, 2)",
        LIGHTING_SETTER
    ));
    assert!(invokes(
        "$lm.Set_Lighting_Current_Status ($id)",
        LIGHTING_SETTER
    ));
    assert!(invokes("[void]$gz.SetSmartFanMode($mode)", MODE_SETTER));
}

#[test]
fn an_invoke_or_module_call_trips_the_detector() {
    assert!(invokes(
        "$lm.Set_Lighting_Current_Status.Invoke(@(4))",
        LIGHTING_SETTER
    ));
    assert!(invokes(
        "Invoke-LenovoWmiMethod -WmiObject $lm -Method 'Set_Lighting_Current_Status' -Property 'x'",
        LIGHTING_SETTER
    ));
    assert!(invokes(
        "Invoke-LenovoWmiMethod -WmiObject $lm -Method \"Set_Lighting_Current_Status\"",
        LIGHTING_SETTER
    ));
    assert!(invokes(
        "Invoke-CimMethod -MethodName 'Set_Lighting_Current_Status'",
        LIGHTING_SETTER
    ));
}

#[test]
fn a_prose_mention_before_a_call_does_not_mask_it() {
    assert!(invokes(
        "Write-Host 'never Set_Lighting_Current_Status'; $lm.Set_Lighting_Current_Status(4)",
        LIGHTING_SETTER
    ));
}

#[test]
fn naming_the_method_in_prose_does_not_trip_it() {
    assert!(!invokes(
        "Write-ToolLog \"never calls Set_Lighting_Current_Status here\"",
        LIGHTING_SETTER
    ));
    assert!(!invokes(
        "methods: Get_Lighting_Current_Status, Set_Lighting_Current_Status",
        LIGHTING_SETTER
    ));
    assert!(!invokes(
        "Write-ToolLog \"Never calls SetSmartFanMode or any lighting setter.\"",
        MODE_SETTER
    ));
    // The getter contains the setter's name minus one letter; not a match.
    assert!(!invokes("try { $r = $gz.GetSmartFanMode() }", MODE_SETTER));
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
    assert!(lines.iter().all(|(_, l)| !invokes(l, LIGHTING_SETTER)));
}

#[test]
fn a_call_outside_comments_survives_stripping() {
    let script = "<# help #>\n$r = $lm.Set_Lighting_Current_Status(4)  # comment after\n";
    let lines = code_lines(script, false);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].0, 2);
    assert!(invokes(&lines[0].1, LIGHTING_SETTER));
}

#[test]
fn rust_line_comments_are_stripped() {
    let source = "// Set_Lighting_Current_Status(1) in a comment\nlet a = 1; // trailing\n";
    let lines = code_lines(source, true);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].1, "let a = 1; ");
}
