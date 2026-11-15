//! `rust clippy`: clippy the given targets locally, showing errors.
//!
//! Runs the `rust_clippy.bxl` BXL -- which materializes each rust
//! target's clippy diagnostics JSON through its `RustcExtraOutputsInfo`
//! provider -- scans the JSON for error-level diagnostics, prints
//! their rendered output, and fails when any is found. The BXL alone
//! cannot gate: the prelude runs the clippy compile infallible
//! (`--failure-filter`) by design, recording diagnostics instead of
//! failing.
//!
//! ```sh
//! buckshot rust clippy tests//rust/...
//! buckshot rust clippy --fix buckshot//buckshot:buckshot
//! ```
//!
//! With `--fix`, machine-applicable suggestions are applied to the
//! sources first (warnings included, like `cargo fix`), then the check
//! re-runs and reports whatever remains.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context;
use clap::Args;

use crate::ci::{Scm, project_root, resolve_buck2};

#[derive(Args)]
pub struct ClippyArgs {
    /// Target labels or patterns to clippy (e.g. `tests//rust/...`).
    /// Only rust targets are checked; other kinds are skipped.
    #[arg(required = true)]
    targets: Vec<String>,

    /// `buck2` executable to invoke for the BXL query. A bare name
    /// (`buck2`) resolves through PATH; a relative path with
    /// directories (`./buck2`) resolves against the project root, so it
    /// works whatever the caller's cwd is.
    #[arg(long, default_value = "buck2")]
    buck2: String,

    /// BXL function materializing clippy diagnostics through the
    /// `RustcExtraOutputsInfo` provider. Defaults to the script shipped
    /// in this repo's `buckshot` cell; override when the cell lives
    /// under another name in a consumer repo.
    #[arg(
        long,
        default_value = "buckshot//rust/clippy/rust_clippy.bxl:rust_clippy"
    )]
    bxl: String,

    /// Apply machine-applicable clippy suggestions to the sources,
    /// then re-check and report whatever remains. Only suggestions
    /// marked `MachineApplicable` are applied; suggestions inside macro
    /// expansions are skipped, as are overlapping ones.
    #[arg(long)]
    fix: bool,
}

/// One rust target's materialized diagnostics.
struct TargetReport {
    label: String,
    diagnostics: Vec<Diagnostic>,
}

pub fn clippy(args: ClippyArgs) -> anyhow::Result<()> {
    let root = project_root(Scm::Auto)?;
    let buck2 = resolve_buck2(&root, &args.buck2);
    let mut pairs = materialize(&buck2, &args.bxl, &root, &args.targets)?;
    if pairs.is_empty() {
        eprintln!("no rust targets to clippy");
        return Ok(());
    }
    if args.fix {
        let reports = load_reports(&pairs)?;
        let fixed = apply_fixes(&root, &reports)?;
        if fixed > 0 {
            eprintln!("applied {fixed} clippy suggestion(s); re-checking...");
            pairs = materialize(&buck2, &args.bxl, &root, &args.targets)?;
        } else {
            eprintln!("no machine-applicable suggestions to apply");
        }
    }
    let reports = load_reports(&pairs)?;
    let (error_count, target_count) = report_errors(&reports);
    if error_count == 0 {
        eprintln!("clippy clean ({} target(s) checked)", reports.len());
        Ok(())
    } else {
        anyhow::bail!("clippy failed with {error_count} error(s) in {target_count} target(s)")
    }
}

/// Run the BXL (which builds the clippy compiles as it materializes
/// their diagnostics) and parse its `<label> <path>` lines.
fn materialize(
    buck2: &str,
    bxl: &str,
    root: &Path,
    targets: &[String],
) -> anyhow::Result<Vec<(String, PathBuf)>> {
    let mut command = Command::new(buck2);
    command.current_dir(root).arg("bxl").arg(bxl).arg("--");
    // The BXL declares `targets` as `cli_args.list(cli_args.string())`,
    // which takes space-separated values after the flag.
    command.arg("--targets");
    for target in targets {
        command.arg(target);
    }
    let output = command
        .stdout(Stdio::piped())
        .output()
        .with_context(|| format!("running `{buck2} bxl {bxl}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`{buck2} bxl {bxl}` failed:\n{stderr}");
    }
    // BXL progress goes to stderr; surface it for diagnosability.
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    Ok(parse_bxl_lines(
        &String::from_utf8(output.stdout).context("non-UTF-8 BXL output")?,
    ))
}

/// One `<label> <path>` line per checked target; blank lines carry no
/// result. The label cannot contain a space, so split on the last one
/// to tolerate spaces in artifact paths.
fn parse_bxl_lines(output: &str) -> Vec<(String, PathBuf)> {
    let mut pairs = Vec::new();
    for line in output.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line.rsplit_once(' ') {
            Some((label, path)) if !label.is_empty() && !path.is_empty() => {
                pairs.push((label.to_string(), PathBuf::from(path)));
            }
            _ => eprintln!("warning: ignoring malformed BXL line: {line}"),
        }
    }
    pairs
}

/// Read every materialized diagnostics file.
fn load_reports(pairs: &[(String, PathBuf)]) -> anyhow::Result<Vec<TargetReport>> {
    pairs
        .iter()
        .map(|(label, path)| {
            let diagnostics = read_diagnostics(path)
                .with_context(|| format!("reading clippy diagnostics for {label}"))?;
            Ok(TargetReport {
                label: label.clone(),
                diagnostics,
            })
        })
        .collect()
}

/// One JSON diagnostic object per line. Malformed lines warn and are
/// skipped; an unreadable file is a hard error.
fn read_diagnostics(path: &Path) -> anyhow::Result<Vec<Diagnostic>> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let mut diagnostics = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_diagnostic_line(line) {
            Some(diagnostic) => diagnostics.push(diagnostic),
            None => eprintln!(
                "warning: ignoring malformed diagnostic line in {}",
                path.display()
            ),
        }
    }
    Ok(diagnostics)
}

fn parse_diagnostic_line(line: &str) -> Option<Diagnostic> {
    serde_json::from_str(line).ok()
}

/// A rustc JSON diagnostic. Only the fields this command needs: level
/// for gating, spans for fixing, `rendered` for display.
#[derive(Debug, serde::Deserialize)]
struct Diagnostic {
    #[serde(default)]
    level: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    rendered: Option<String>,
    #[serde(default)]
    spans: Vec<Span>,
    #[serde(default)]
    children: Vec<ChildDiagnostic>,
}

#[derive(Debug, serde::Deserialize)]
struct ChildDiagnostic {
    #[serde(default)]
    spans: Vec<Span>,
}

#[derive(Debug, serde::Deserialize)]
struct Span {
    #[serde(default)]
    file_name: String,
    #[serde(default)]
    byte_start: usize,
    #[serde(default)]
    byte_end: usize,
    #[serde(default)]
    suggested_replacement: Option<String>,
    #[serde(default)]
    suggestion_applicability: Option<String>,
    #[serde(default)]
    expansion: Option<serde_json::Value>,
}

/// Print every error-level diagnostic's rendered output. Returns the
/// error count and the number of targets that have at least one.
fn report_errors(reports: &[TargetReport]) -> (usize, usize) {
    let mut error_count = 0;
    let mut target_count = 0;
    for report in reports {
        let mut report_errors = 0;
        for diagnostic in &report.diagnostics {
            if diagnostic.level != "error" {
                continue;
            }
            match &diagnostic.rendered {
                // `rendered` already ends in a newline.
                Some(rendered) => print!("{rendered}"),
                None => println!("{}: {}", report.label, diagnostic.message),
            }
            report_errors += 1;
        }
        if report_errors > 0 {
            target_count += 1;
            error_count += report_errors;
        }
    }
    (error_count, target_count)
}

/// One machine-applied edit: replace `source[start..end]` with
/// `replacement`. Byte offsets are relative to the file rustc saw.
struct Replacement {
    file: String,
    start: usize,
    end: usize,
    replacement: String,
}

/// Collect machine-applicable suggestions from every diagnostic
/// (errors and warnings alike, like `cargo fix`). Only suggestions
/// marked `MachineApplicable` whose spans lie in plain source files
/// qualify; suggestions inside macro expansions are skipped.
fn collect_replacements(diagnostics: &[Diagnostic]) -> Vec<Replacement> {
    let mut replacements = Vec::new();
    for diagnostic in diagnostics {
        for span in &diagnostic.spans {
            push_if_fixable(&mut replacements, span);
        }
        for child in &diagnostic.children {
            for span in &child.spans {
                push_if_fixable(&mut replacements, span);
            }
        }
    }
    replacements
}

fn push_if_fixable(replacements: &mut Vec<Replacement>, span: &Span) {
    let (Some(replacement), Some(applicability)) =
        (&span.suggested_replacement, &span.suggestion_applicability)
    else {
        return;
    };
    if applicability != "MachineApplicable" {
        return;
    }
    if span.expansion.is_some() {
        return;
    }
    if span.file_name.is_empty() {
        return;
    }
    replacements.push(Replacement {
        file: span.file_name.clone(),
        start: span.byte_start,
        end: span.byte_end,
        replacement: replacement.clone(),
    });
}

/// Apply every suggestion whose file lives under the project root.
/// Returns the number of applied suggestions.
fn apply_fixes(root: &Path, reports: &[TargetReport]) -> anyhow::Result<usize> {
    let mut by_file: BTreeMap<String, Vec<Replacement>> = BTreeMap::new();
    for report in reports {
        for replacement in collect_replacements(&report.diagnostics) {
            by_file
                .entry(replacement.file.clone())
                .or_default()
                .push(replacement);
        }
    }
    let mut total = 0;
    for (file, replacements) in &by_file {
        if Path::new(file).is_absolute() {
            eprintln!("warning: skipping fix outside the checkout: {file}");
            continue;
        }
        let path = root.join(file);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                eprintln!("warning: skipping fix for unreadable {file}: {error}");
                continue;
            }
        };
        let (fixed, applied, skipped) = apply_replacements(&content, replacements);
        if applied == 0 {
            continue;
        }
        std::fs::write(&path, fixed)
            .with_context(|| format!("writing fixes to {}", path.display()))?;
        eprintln!("fixed {applied} suggestion(s) in {file}");
        if skipped > 0 {
            eprintln!("warning: skipped {skipped} overlapping suggestion(s) in {file}");
        }
        total += applied;
    }
    Ok(total)
}

/// Splice replacements into file content, latest span first so
/// earlier offsets stay valid. Overlapping, out-of-bounds, and
/// non-character-boundary replacements are skipped rather than
/// applied. Returns the new content plus applied/skipped counts.
fn apply_replacements(content: &str, replacements: &[Replacement]) -> (String, usize, usize) {
    let mut ordered: Vec<&Replacement> = replacements.iter().collect();
    ordered.sort_by(|a, b| b.start.cmp(&a.start).then(b.end.cmp(&a.end)));
    let mut fixed = content.to_string();
    // Ranges already spliced, in original coordinates: every later
    // splice lies entirely below them, so original coordinates stay
    // comparable throughout.
    let mut applied: Vec<(usize, usize)> = Vec::new();
    let (mut applied_count, mut skipped) = (0, 0);
    for replacement in ordered {
        let valid = replacement.start <= replacement.end
            && replacement.end <= fixed.len()
            && fixed.is_char_boundary(replacement.start)
            && fixed.is_char_boundary(replacement.end);
        let overlaps = applied
            .iter()
            .any(|&(start, end)| replacement.start < end && start < replacement.end);
        if !valid || overlaps {
            skipped += 1;
            continue;
        }
        fixed.replace_range(replacement.start..replacement.end, &replacement.replacement);
        applied.push((replacement.start, replacement.end));
        applied_count += 1;
    }
    (fixed, applied_count, skipped)
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(clap::Parser)]
    struct TestCli {
        #[command(flatten)]
        args: ClippyArgs,
    }

    #[test]
    fn defaults_to_buck2_and_bundled_bxl() {
        let cli = TestCli::try_parse_from(["clippy", "tests//rust/..."]).unwrap();
        assert_eq!(cli.args.targets, vec!["tests//rust/...".to_string()]);
        assert_eq!(cli.args.buck2, "buck2");
        assert_eq!(
            cli.args.bxl,
            "buckshot//rust/clippy/rust_clippy.bxl:rust_clippy"
        );
        assert!(!cli.args.fix);
    }

    #[test]
    fn fix_and_overrides_parse() {
        let cli = TestCli::try_parse_from([
            "clippy",
            "--fix",
            "--buck2",
            "./buck2",
            "--bxl",
            "other//clippy.bxl:clippy",
            "tests//rust:hello",
            "tests//python:hello",
        ])
        .unwrap();
        assert!(cli.args.fix);
        assert_eq!(cli.args.buck2, "./buck2");
        assert_eq!(cli.args.bxl, "other//clippy.bxl:clippy");
        assert_eq!(cli.args.targets.len(), 2);
    }

    #[test]
    fn targets_are_required() {
        assert!(TestCli::try_parse_from(["clippy"]).is_err());
    }

    #[test]
    fn bxl_lines_split_label_from_path() {
        let pairs = parse_bxl_lines(
            "tests//rust:hello buck-out/x/hello-clippy-clippy.json\n\
             \n\
             buckshot//buckshot:buckshot buck-out/y/buckshot-clippy-clippy.json\n",
        );
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].0, "tests//rust:hello");
        assert_eq!(
            pairs[0].1,
            PathBuf::from("buck-out/x/hello-clippy-clippy.json")
        );
        // Malformed lines warn and are skipped.
        assert!(parse_bxl_lines("nospaceshere\n").is_empty());
    }

    fn diagnostic_json(
        level: &str,
        file: &str,
        start: usize,
        end: usize,
        replacement: Option<&str>,
        applicability: Option<&str>,
    ) -> String {
        let span = serde_json::json!({
            "file_name": file,
            "byte_start": start,
            "byte_end": end,
            "suggested_replacement": replacement,
            "suggestion_applicability": applicability,
            "expansion": null,
            "is_primary": true,
        });
        serde_json::json!({
            "level": level,
            "message": "test diagnostic",
            "rendered": "test diagnostic\n",
            "code": {"code": "clippy::test", "explanation": null},
            "spans": [span],
            "children": [],
        })
        .to_string()
    }

    #[test]
    fn error_levels_gate_and_malformed_lines_skip() {
        let error = parse_diagnostic_line(
            &diagnostic_json("error", "a.rs", 0, 1, None, None),
        )
        .unwrap();
        assert_eq!(error.level, "error");
        let warning = parse_diagnostic_line(
            &diagnostic_json("warning", "a.rs", 0, 1, None, None),
        )
        .unwrap();
        assert_eq!(warning.level, "warning");
        assert!(parse_diagnostic_line("not json").is_none());
        assert!(parse_diagnostic_line("").is_none());
    }

    #[test]
    fn only_machine_applicable_plain_spans_collect() {
        let diags = vec![
            parse_diagnostic_line(
                &diagnostic_json("error", "a.rs", 0, 1, Some("b"), Some("MachineApplicable")),
            )
            .unwrap(),
            // MaybeIncorrect stays manual.
            parse_diagnostic_line(
                &diagnostic_json("warning", "a.rs", 2, 3, Some("c"), Some("MaybeIncorrect")),
            )
            .unwrap(),
            // Macro expansions are never touched.
            {
                let mut raw: serde_json::Value = serde_json::from_str(
                    &diagnostic_json("error", "a.rs", 4, 5, Some("d"), Some("MachineApplicable")),
                )
                .unwrap();
                raw["spans"][0]["expansion"] = serde_json::json!({"span": null});
                parse_diagnostic_line(&raw.to_string()).unwrap()
            },
            // No replacement, nothing to apply.
            parse_diagnostic_line(
                &diagnostic_json("error", "a.rs", 6, 7, None, Some("MachineApplicable")),
            )
            .unwrap(),
        ];
        let replacements = collect_replacements(&diags);
        assert_eq!(replacements.len(), 1);
        assert_eq!(replacements[0].file, "a.rs");
        assert_eq!((replacements[0].start, replacements[0].end), (0, 1));
        assert_eq!(replacements[0].replacement, "b");
    }

    fn replacement(file: &str, start: usize, end: usize, text: &str) -> Replacement {
        Replacement {
            file: file.to_string(),
            start,
            end,
            replacement: text.to_string(),
        }
    }

    #[test]
    fn replacements_apply_latest_first() {
        let (fixed, applied, skipped) = apply_replacements(
            "aaa bbb ccc",
            &[
                replacement("f.rs", 0, 3, "AAA"),
                replacement("f.rs", 8, 11, "CCC"),
            ],
        );
        assert_eq!(fixed, "AAA bbb CCC");
        assert_eq!((applied, skipped), (2, 0));
    }

    #[test]
    fn disjoint_replacements_all_apply() {
        let (fixed, applied, skipped) = apply_replacements(
            "aaa bbb ccc",
            &[
                replacement("f.rs", 0, 3, "AAA"),
                // Zero-width insertion between the others: applies.
                replacement("f.rs", 4, 4, "-"),
                replacement("f.rs", 8, 11, "CCC"),
            ],
        );
        assert_eq!(fixed, "AAA -bbb CCC");
        assert_eq!((applied, skipped), (3, 0));
    }

    #[test]
    fn overlapping_and_invalid_replacements_skip() {
        let (fixed, applied, skipped) = apply_replacements(
            "aaa bbb",
            &[
                replacement("f.rs", 0, 7, "ALL"),
                // Fully inside the first: skipped.
                replacement("f.rs", 2, 5, "MID"),
                // Out of bounds: skipped.
                replacement("f.rs", 0, 99, "TOO FAR"),
            ],
        );
        assert_eq!(fixed, "aaMIDbb");
        assert_eq!((applied, skipped), (1, 2));
    }

    #[test]
    fn multibyte_boundaries_skip() {
        // "é" is two bytes; byte 1 is not a character boundary.
        let (fixed, applied, skipped) = apply_replacements(
            "aé",
            &[replacement("f.rs", 1, 2, "e")],
        );
        assert_eq!(fixed, "aé");
        assert_eq!((applied, skipped), (0, 1));
    }
}
