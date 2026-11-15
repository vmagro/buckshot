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
//! re-runs and reports whatever remains. With `--output github`,
//! results print as `file,line` annotations (plus inline PR review
//! comments when a PR context exists) and the action outputs record
//! into `--github-output` -- this is what the `rust/clippy` action runs.

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

    /// How to report the results. `local` prints each error's rendered
    /// output for a terminal; `github` prints `file,line` check
    /// annotations (plus inline PR review comments, unless disabled)
    /// and records the action outputs when `GITHUB_OUTPUT` is set.
    #[arg(long, value_enum, default_value_t = Output::Local)]
    output: Output,

    /// Post inline PR review comments on the exact lines clippy flags,
    /// when a PR context is available and the `gh` CLI plus
    /// `pull-requests: write` are present. Failures warn but never fail
    /// the run. Only meaningful with `--output github`.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    inline_comments: bool,

    /// Where to append the `targets`/`count`/`checked` action outputs.
    /// Under `buck2 run` the environment is scrubbed, so the action
    /// passes its own `$GITHUB_OUTPUT` through here instead of relying
    /// on the variable reaching the binary. Falls back to
    /// `$GITHUB_OUTPUT` when set, and does nothing otherwise. Only
    /// meaningful with `--output github`.
    #[arg(long)]
    github_output: Option<PathBuf>,
}

/// Reporting mode for clippy results.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
enum Output {
    /// Human-readable: rendered diagnostics to stdout, status to stderr.
    #[default]
    Local,
    /// GitHub Actions: `file,line` annotations, optional inline PR
    /// review comments, and action outputs via `GITHUB_OUTPUT`.
    Github,
}

/// One rust target's materialized diagnostics.
struct TargetReport {
    label: String,
    diagnostics: Vec<Diagnostic>,
}

pub fn clippy(args: ClippyArgs) -> anyhow::Result<()> {
    let root = project_root(Scm::Auto)?;
    let buck2 = resolve_buck2(&root, &args.buck2);
    // The action passes its own `$GITHUB_OUTPUT` explicitly because
    // `buck2 run` scrubs the binary's environment.
    let github_output = args.github_output.clone().or_else(|| {
        std::env::var("GITHUB_OUTPUT")
            .ok()
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
    });
    let mut pairs = materialize(&buck2, &args.bxl, &root, &args.targets)?;
    if pairs.is_empty() {
        eprintln!("no rust targets to clippy");
        if args.output == Output::Github {
            write_github_outputs(&[], github_output.as_deref())?;
        }
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
    match args.output {
        Output::Local => {
            let (error_count, target_count) = report_errors(&reports);
            if error_count == 0 {
                eprintln!("clippy clean ({} target(s) checked)", reports.len());
                Ok(())
            } else {
                anyhow::bail!(
                    "clippy failed with {error_count} error(s) in {target_count} target(s)"
                )
            }
        }
        Output::Github => github_report(&reports, args.inline_comments, github_output.as_deref()),
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
    code: Option<DiagnosticCode>,
    #[serde(default)]
    spans: Vec<Span>,
    #[serde(default)]
    children: Vec<ChildDiagnostic>,
}

#[derive(Debug, serde::Deserialize)]
struct DiagnosticCode {
    #[serde(default)]
    code: Option<String>,
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
    line_start: usize,
    #[serde(default)]
    line_end: usize,
    #[serde(default)]
    is_primary: bool,
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

/// Print `file,line` annotations for every error diagnostic, post
/// inline PR review comments when possible, record the action outputs,
/// and fail when any error was found.
fn github_report(
    reports: &[TargetReport],
    inline_comments: bool,
    github_output: Option<&Path>,
) -> anyhow::Result<()> {
    let mut failures = 0;
    let mut failed_targets = 0;
    let mut commentable = Vec::new();
    for report in reports {
        let mut report_failures = 0;
        for diagnostic in &report.diagnostics {
            if diagnostic.level != "error" {
                continue;
            }
            let code = error_code(diagnostic);
            let location = error_location(diagnostic);
            println!("{}", annotation(code, &diagnostic.message, location));
            if let Some((path, line, end_line)) = location {
                commentable.push((path, line, end_line, code, diagnostic.message.as_str()));
            }
            report_failures += 1;
        }
        if report_failures > 0 {
            failed_targets += 1;
            failures += report_failures;
        }
    }
    if inline_comments && !commentable.is_empty() {
        post_inline_comments(&commentable);
    }
    write_github_outputs(reports, github_output)?;
    if failures == 0 {
        eprintln!("clippy clean ({} target(s) checked)", reports.len());
        Ok(())
    } else {
        eprintln!("clippy failed with {failures} error(s) in {failed_targets} target(s)");
        anyhow::bail!("clippy failed with {failures} error(s) in {failed_targets} target(s)")
    }
}

/// The lint code, defaulting to plain `clippy` for rustc diagnostics.
fn error_code(diagnostic: &Diagnostic) -> &str {
    diagnostic
        .code
        .as_ref()
        .and_then(|code| code.code.as_deref())
        .unwrap_or("clippy")
}

/// Where an error diagnostic points: its primary span, falling back
/// to its first span. Spanless diagnostics (e.g. "aborting due to N
/// previous errors") have no location.
fn error_location(diagnostic: &Diagnostic) -> Option<(&str, usize, usize)> {
    let span = diagnostic
        .spans
        .iter()
        .find(|span| span.is_primary)
        .or_else(|| diagnostic.spans.first())?;
    if span.file_name.is_empty() || span.line_start == 0 {
        return None;
    }
    let end_line = if span.line_end == 0 {
        span.line_start
    } else {
        span.line_end
    };
    Some((span.file_name.as_str(), span.line_start, end_line))
}

/// A `::error` workflow command. GitHub renders the `file,line` form
/// inline on the exact lines, in PR diffs and on push commit pages.
fn annotation(code: &str, message: &str, location: Option<(&str, usize, usize)>) -> String {
    match location {
        Some((path, line, end_line)) => format!(
            "::error file={path},line={line},endLine={end_line},title={}::{}",
            escape(code),
            escape(message),
        ),
        None => format!("::error title={}::{}", escape(code), escape(message)),
    }
}

fn escape(text: &str) -> String {
    text.replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

/// Append the action outputs to `path` (the action's `$GITHUB_OUTPUT`,
/// passed explicitly since `buck2 run` scrubs the environment).
/// A missing path means a plain local run: nothing to record.
fn write_github_outputs(reports: &[TargetReport], path: Option<&Path>) -> anyhow::Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    use std::io::Write;
    file.write_all(github_outputs_block(reports).as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn github_outputs_block(reports: &[TargetReport]) -> String {
    let mut block = String::from("targets<<EOF\n");
    for report in reports {
        block.push_str(&report.label);
        block.push('\n');
    }
    block.push_str("EOF\n");
    block.push_str(&format!("count={}\n", reports.len()));
    block.push_str(&format!("checked={}\n", !reports.is_empty()));
    block
}

/// PR review-comment context: the repo, the PR number, and the commit
/// the comments attach to.
struct PrContext {
    repo: String,
    number: String,
    sha: String,
}

fn pr_context() -> Option<PrContext> {
    let repo = std::env::var("GH_REPOSITORY").ok().filter(|s| !s.is_empty())?;
    if std::env::var("GH_EVENT_NAME").as_deref() == Ok("pull_request") {
        // The tested commit on PR events is the PR head: the default
        // merge-commit checkout is not part of the PR, so review
        // comments must name the head SHA, not `github.sha`.
        let number = std::env::var("GH_PR_NUMBER").ok().filter(|s| !s.is_empty())?;
        let sha = std::env::var("GH_HEAD_SHA").ok().filter(|s| !s.is_empty())?;
        return Some(PrContext { repo, number, sha });
    }
    // Pushes (and manual runs) have no PR number in the event: comment
    // only when exactly one open PR contains the pushed SHA.
    let sha = std::env::var("GH_HEAD_SHA").ok().filter(|s| !s.is_empty())?;
    let pulls = gh_api(&[
        format!("repos/{repo}/commits/{sha}/pulls"),
        "--jq".to_string(),
        "[.[] | select(.state == \"open\") | .number]".to_string(),
    ])?;
    match pick_associated_pr(&pulls) {
        Some(number) => Some(PrContext {
            repo,
            number: number.to_string(),
            sha,
        }),
        None => {
            eprintln!(
                "no PR context for inline comments (push without an associated open PR): annotations above still mark the exact lines"
            );
            None
        }
    }
}

/// The associated-PR lookup succeeds only with exactly one open PR:
/// zero means a bare push, several means ambiguous.
fn pick_associated_pr(pulls: &serde_json::Value) -> Option<u64> {
    let numbers: Vec<u64> = serde_json::from_value(pulls.clone()).ok()?;
    if numbers.len() == 1 { Some(numbers[0]) } else { None }
}

/// `gh api ...`, returning parsed JSON or None on any failure
/// (missing `gh`, no network, non-zero exit, unparsable output).
fn gh_api(args: &[String]) -> Option<serde_json::Value> {
    let output = Command::new("gh").arg("api").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

fn gh_available() -> bool {
    Command::new("gh")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Post one inline review comment per error location. Best-effort:
/// failures warn but never fail the run.
fn post_inline_comments(errors: &[(&str, usize, usize, &str, &str)]) {
    let Some(context) = pr_context() else {
        return;
    };
    if !gh_available() {
        eprintln!("warning: `gh` not found, skipping inline PR comments");
        return;
    }
    let existing = fetch_comments(&context);
    let run_url = std::env::var("GH_RUN_URL").unwrap_or_default();
    for (path, line, end_line, code, message) in errors {
        if already_commented(&existing, path, *line, code) {
            continue;
        }
        post_comment(&context, &run_url, path, *line, *end_line, code, message);
    }
}

#[derive(Debug, serde::Deserialize)]
struct ExistingComment {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    line: Option<usize>,
    #[serde(default)]
    body: Option<String>,
}

fn fetch_comments(context: &PrContext) -> Vec<ExistingComment> {
    let comments = gh_api(&[
        format!("repos/{}/pulls/{}/comments", context.repo, context.number),
        "--paginate".to_string(),
        "--jq".to_string(),
        "[.[] | {path: .path, line: .line, body: .body}]".to_string(),
    ])
    .and_then(|value| serde_json::from_value(value).ok());
    comments.unwrap_or_default()
}

const COMMENT_MARKER: &str = "buckshot-rust-clippy";

fn comment_marker(code: &str) -> String {
    format!("<!-- {COMMENT_MARKER}:{code} -->")
}

fn comment_body(code: &str, message: &str, run_url: &str) -> String {
    let mut body = format!("**`{code}`** — {message}\n\n<sub>Reported by buckshot `rust-clippy`");
    if !run_url.is_empty() {
        body.push_str(&format!(" · [run]({run_url})"));
    }
    body.push_str("</sub>\n");
    body.push_str(&comment_marker(code));
    body
}

/// An identical marker comment already on this path and line: don't
/// re-post on every push.
fn already_commented(comments: &[ExistingComment], path: &str, line: usize, code: &str) -> bool {
    let marker = comment_marker(code);
    comments.iter().any(|comment| {
        comment.path.as_deref() == Some(path)
            && comment.line == Some(line)
            && comment.body.as_deref().is_some_and(|body| body.contains(&marker))
    })
}

fn post_comment(
    context: &PrContext,
    run_url: &str,
    path: &str,
    line: usize,
    end_line: usize,
    code: &str,
    message: &str,
) {
    let mut args = vec![
        format!("repos/{}/pulls/{}/comments", context.repo, context.number),
        "-f".to_string(),
        format!("commit_id={}", context.sha),
        "-f".to_string(),
        format!("path={path}"),
        "-f".to_string(),
        format!("line={line}"),
        "-f".to_string(),
        "side=RIGHT".to_string(),
        "-f".to_string(),
        "subject_type=line".to_string(),
        "-f".to_string(),
        format!("body={}", comment_body(code, message, run_url)),
    ];
    if end_line != line {
        args.push("-f".to_string());
        args.push(format!("start_line={line}"));
        args.push("-f".to_string());
        args.push("start_side=RIGHT".to_string());
    }
    match Command::new("gh").arg("api").args(&args).output() {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let tail = String::from_utf8_lossy(&output.stderr)
                .lines()
                .last()
                .unwrap_or("")
                .to_string();
            eprintln!(
                "warning: could not comment on {path}:{line} of PR #{} ({tail}); \
                is the line in the PR diff and does the token have pull-requests: write?",
                context.number,
            );
        }
        Err(error) => {
            eprintln!(
                "warning: could not run `gh api` to comment on PR #{}: {error}",
                context.number,
            );
        }
    }
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
    fn output_flag_defaults_to_local() {
        let cli = TestCli::try_parse_from(["clippy", "tests//rust/..."]).unwrap();
        assert_eq!(cli.args.output, Output::Local);
        assert!(cli.args.inline_comments);
        let cli = TestCli::try_parse_from([
            "clippy",
            "--output",
            "github",
            "--inline-comments=false",
            "tests//rust/...",
        ])
        .unwrap();
        assert_eq!(cli.args.output, Output::Github);
        assert!(!cli.args.inline_comments);
        assert!(TestCli::try_parse_from(["clippy", "--output", "json", "t//:t"]).is_err());
        let cli = TestCli::try_parse_from([
            "clippy",
            "--github-output",
            "/tmp/outputs.txt",
            "t//:t",
        ])
        .unwrap();
        assert_eq!(
            cli.args.github_output,
            Some(PathBuf::from("/tmp/outputs.txt"))
        );
    }

    fn located_json(code: Option<&str>, line: usize, end_line: usize) -> String {
        serde_json::json!({
            "level": "error",
            "message": "a 100% broken\nthing",
            "rendered": null,
            "code": code.map(|c| serde_json::json!({"code": c, "explanation": null})),
            "spans": [{
                "file_name": "a.rs",
                "byte_start": 0,
                "byte_end": 1,
                "line_start": line,
                "line_end": end_line,
                "is_primary": true,
            }],
            "children": [],
        })
        .to_string()
    }

    #[test]
    fn annotations_escape_and_fall_back_without_location() {
        let located = parse_diagnostic_line(&located_json(Some("clippy::x"), 3, 5)).unwrap();
        assert_eq!(error_code(&located), "clippy::x");
        assert_eq!(
            annotation(error_code(&located), &located.message, error_location(&located)),
            "::error file=a.rs,line=3,endLine=5,title=clippy::x::a 100%25 broken%0Athing",
        );
        let spanless = Diagnostic {
            level: "error".to_string(),
            message: "boom".to_string(),
            rendered: None,
            code: None,
            spans: vec![],
            children: vec![],
        };
        assert_eq!(error_code(&spanless), "clippy");
        assert_eq!(
            annotation(error_code(&spanless), &spanless.message, error_location(&spanless)),
            "::error title=clippy::boom",
        );
    }

    #[test]
    fn associated_pr_needs_exactly_one_open() {
        assert_eq!(pick_associated_pr(&serde_json::json!([7])), Some(7));
        assert_eq!(pick_associated_pr(&serde_json::json!([])), None);
        assert_eq!(pick_associated_pr(&serde_json::json!([7, 8])), None);
        assert_eq!(pick_associated_pr(&serde_json::json!("nope")), None);
    }

    #[test]
    fn identical_marker_comments_are_skipped() {
        let comments = vec![ExistingComment {
            path: Some("a.rs".to_string()),
            line: Some(3),
            body: Some(format!("old <!-- {COMMENT_MARKER}:clippy::x -->")),
        }];
        assert!(already_commented(&comments, "a.rs", 3, "clippy::x"));
        assert!(!already_commented(&comments, "a.rs", 4, "clippy::x"));
        assert!(!already_commented(&comments, "a.rs", 3, "clippy::y"));
        assert!(!already_commented(&comments, "b.rs", 3, "clippy::x"));
    }

    #[test]
    fn comment_body_links_run_and_marks_code() {
        let body = comment_body("clippy::x", "msg", "http://run/1");
        assert!(body.contains("**`clippy::x`**"));
        assert!(body.contains("[run](http://run/1)"));
        assert!(body.contains(&comment_marker("clippy::x")));
        assert!(!comment_body("clippy::x", "msg", "").contains("[run]("));
    }

    #[test]
    fn github_outputs_block_lists_checked_targets() {
        let reports = vec![
            TargetReport {
                label: "a//:b".to_string(),
                diagnostics: vec![],
            },
            TargetReport {
                label: "c//:d".to_string(),
                diagnostics: vec![],
            },
        ];
        assert_eq!(
            github_outputs_block(&reports),
            "targets<<EOF\na//:b\nc//:d\nEOF\ncount=2\nchecked=true\n",
        );
        assert_eq!(
            github_outputs_block(&[]),
            "targets<<EOF\nEOF\ncount=0\nchecked=false\n",
        );
    }

    #[test]
    fn github_outputs_append_to_file() {
        let path = std::env::temp_dir().join(format!(
            "buckshot-clippy-test-outputs-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let reports = vec![TargetReport {
            label: "a//:b".to_string(),
            diagnostics: vec![],
        }];
        write_github_outputs(&reports, Some(&path)).unwrap();
        // A missing path (plain local run) records nothing and fails nothing.
        write_github_outputs(&reports, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "targets<<EOF\na//:b\nEOF\ncount=1\nchecked=true\n",
        );
        let _ = std::fs::remove_file(&path);
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
