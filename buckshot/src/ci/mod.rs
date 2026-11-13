//! `ci changed-targets`: buck targets affected by SCM changes since a base revision.
//!
//! Derives the changed files from git (or Sapling/`sl`), resolves the
//! targets that own them plus all of their reverse dependencies through
//! the `affected_targets` BXL script (which runs `cquery owner` followed
//! by `cquery rdeps`), and prints one unconfigured target label per line
//! so the output feeds directly into `buck2 test`:
//!
//! ```sh
//! buck2 test $(buckshot ci changed-targets --base origin/main)
//! ```
//!
//! With no `--universe` the printed set covers every local cell (found
//! via `buck2 audit cell`), so the default is "all changed targets in the
//! repo" -- note that is not the same as `//...`, which only covers the
//! main cell. Pass `--universe <pattern>` (repeatable) to scope the output
//! down, e.g. `--universe tests//...`, and `--only <pattern>` to narrow it
//! further. The graph query itself always spans everything (so in-scope
//! reverse dependencies of out-of-scope owners are still found); scoping
//! filters what gets printed.
//!
//! With no affected targets nothing is printed and the command still
//! exits 0, so `buck2 test` must be skipped on empty output (an empty
//! argument list would test far more than intended). The changed files
//! themselves go to stderr for log visibility; only target labels go to
//! stdout.

use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;

use anyhow::Context;
use clap::Args;

/// Which source-control tool provides the changed-file list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
enum Scm {
    /// Use `sl` inside a Sapling checkout, `git` otherwise.
    #[default]
    Auto,
    Git,
    Sl,
}

#[derive(Args)]
pub struct ChangedTargetsArgs {
    /// Read the changed-file list from this file (one path per line)
    /// instead of deriving it from the SCM range. For CI driven by the
    /// GitHub API (which lists PR files without needing any git history).
    /// `--uncommitted` still appends working-copy files on top.
    #[arg(long, value_name = "PATH")]
    files_from: Option<PathBuf>,

    /// SCM revision to compare against (exclusive): files changed in
    /// `(base, head]` count. For PR CI pass the base branch (e.g.
    /// `origin/main`); for push CI pass the previous commit (`before` SHA).
    /// Ignored when `--all-files` or `--files-from` is given.
    #[arg(long, default_value = "origin/main")]
    base: String,

    /// Treat every tracked file as changed, skipping the `(base, head]`
    /// range entirely. For push CI on a brand-new branch (whose `before`
    /// SHA is all zeros and names no commit), where anything less would
    /// need a guessed base.
    #[arg(long)]
    all_files: bool,

    /// Head revision of the range (inclusive).
    #[arg(long, default_value = "HEAD")]
    head: String,

    /// Compare against `base` verbatim instead of resolving it to the
    /// merge-base of `base` and `head` first. Merge-base is what makes a
    /// stale branch compare against its fork point rather than flagging
    /// everything landed since.
    #[arg(long)]
    no_merge_base: bool,

    /// Which SCM to query for the file list.
    #[arg(long, value_enum, default_value_t = Scm::Auto)]
    scm: Scm,

    /// Also include uncommitted working-copy changes: tracked files
    /// differing from `HEAD` plus new (non-ignored) files. Off by default:
    /// CI checks out clean trees, where this would only add noise.
    #[arg(long)]
    uncommitted: bool,

    /// Only print affected targets within these patterns (repeatable).
    /// Defaults to one `<cell>//...` per local cell (found via `buck2
    /// audit cell`), i.e. every target in the repo -- note that is broader
    /// than `//...`, which only covers the main cell. Narrow to scope a CI
    /// run, e.g. `--universe tests//...`. The query itself always spans the
    /// full graph (so in-scope reverse dependencies of out-of-scope owners
    /// are still found); this filters what gets printed.
    ///
    /// Pattern forms: `cell//...`, `cell//path/...`, `cell//path:name`, or
    /// `cell//path:` (whole package). A pattern without a cell matches in
    /// any cell.
    #[arg(long = "universe")]
    universe: Vec<String>,

    /// Only print affected targets matching these patterns (repeatable,
    /// same forms as `--universe`). Intersects with `--universe`; empty
    /// (the default) means no further narrowing.
    #[arg(long = "only")]
    only: Vec<String>,

    /// `buck2` executable to invoke for the audit-cell and BXL queries.
    /// A bare name (`buck2`) resolves through PATH; a relative path with
    /// directories (`./buck2`) resolves against the project root, so it
    /// works whatever the caller's cwd is.
    #[arg(long, default_value = "buck2")]
    buck2: String,

    /// BXL function implementing the owner + rdeps query. Defaults to the
    /// script shipped in this repo's `buckshot` cell; override when the
    /// cell lives under another name in a consumer repo.
    #[arg(long, default_value = "buckshot//scripts/affected_targets.bxl:affected_targets")]
    bxl: String,
}

/// How many changed files get logged to stderr before the listing cuts
/// off with a count. The query always sees every file.
const MAX_LOGGED_FILES: usize = 100;

/// Concrete SCM choice after resolving `Auto`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolvedScm {
    Git,
    Sl,
}

/// Anchor the buck2 executable: a bare name goes through PATH lookup and
/// an absolute path is used as-is, but a relative path with directories
/// (the `./buck2` dotslash wrapper) resolves against the project root --
/// child processes spawn with the root as cwd, yet relative program paths
/// resolve against the caller's cwd, so an unanchored `./buck2` breaks
/// from any subdirectory (and a bare `buck2` may not exist at all, e.g.
/// on CI runners).
fn resolve_buck2(root: &std::path::Path, buck2: &str) -> String {
    let path = std::path::Path::new(buck2);
    if path.is_absolute() || path.components().count() == 1 {
        buck2.to_string()
    } else {
        root.join(path).to_string_lossy().into_owned()
    }
}

pub fn run(args: ChangedTargetsArgs) -> anyhow::Result<()> {
    let root = project_root(args.scm)?;
    let buck2 = resolve_buck2(&root, &args.buck2);
    let scm = match args.scm {
        Scm::Git => ResolvedScm::Git,
        Scm::Sl => ResolvedScm::Sl,
        Scm::Auto => detect_scm(&root),
    };
    let mut files = match &args.files_from {
        Some(path) => read_files_list(path)?,
        None => changed_files(
            scm,
            &root,
            &args.base,
            &args.head,
            !args.no_merge_base,
            args.uncommitted,
            args.all_files,
        )?,
    };
    if args.files_from.is_some() && args.uncommitted {
        // Same working-copy fold-in as the range modes get.
        files.extend(uncommitted_files(scm, &root)?);
    }
    files.sort();
    files.dedup();
    if files.is_empty() {
        eprintln!("no changed files since {}", args.base);
        return Ok(());
    }
    // Cap the listing: an --all-files run logs every tracked file, which
    // drowns CI logs in tens of thousands of lines. The full list still
    // flows to the query; only the log is truncated.
    eprintln!("changed files ({}):", files.len());
    for file in files.iter().take(MAX_LOGGED_FILES) {
        eprintln!("  {file}");
    }
    if files.len() > MAX_LOGGED_FILES {
        eprintln!("  ... and {} more", files.len() - MAX_LOGGED_FILES);
    }

    let cells = audit_cells(&buck2, &root)?;
    let universe = match args.universe.is_empty() {
        false => args.universe.clone(),
        // Only cells whose path is an existing directory count, which
        // skips bundled/nonexistent cells like `prelude`.
        true => {
            let universe = universe_from_cells(&cells, &root);
            if universe.is_empty() {
                anyhow::bail!("`{buck2} audit cell --json` listed no local cells");
            }
            universe
        }
    };
    eprintln!("universe ({}):", universe.len());
    for pattern in &universe {
        eprintln!("  {pattern}");
    }
    let universe_patterns = universe
        .iter()
        .map(|pattern| TargetPattern::parse(pattern))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let only = args
        .only
        .iter()
        .map(|pattern| TargetPattern::parse(pattern))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let files = qualify_files(&files, &cells, &root);
    let targets = query_affected(&buck2, &args.bxl, &root, &files, &universe)?;
    // The BXL query spans the full graph regardless of universe (this
    // buck2's `owner`/`rdeps` take it as configuration context, not a
    // bound), so scope is enforced here: keep affected targets inside the
    // universe, narrowed further by `--only`. In-scope reverse
    // dependencies of out-of-scope owners survive, which traversal-side
    // bounding would miss.
    let targets = filter_targets(targets, &universe_patterns, &only);
    for target in &targets {
        println!("{target}");
    }
    eprintln!("affected targets ({}):", targets.len());
    Ok(())
}

/// Every cell's `{name: absolute path}` from `buck2 audit cell --json`,
/// backing both universe discovery and file-to-cell qualification.
fn audit_cells(
    buck2: &str,
    root: &std::path::Path,
) -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let output = Command::new(buck2)
        .current_dir(root)
        .args(["audit", "cell", "--json"])
        .output()
        .with_context(|| format!("running `{buck2} audit cell --json`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`{buck2} audit cell --json` failed:\n{stderr}");
    }
    serde_json::from_slice(&output.stdout).context("parsing `buck2 audit cell --json` output")
}

/// One `<cell>//...` per cell whose path is an existing directory, sorted
/// and deduplicated. Nonexistent paths (bundled cells like `prelude`,
/// placeholder cells like `none`) drop out here.
fn universe_from_cells(
    cells: &std::collections::BTreeMap<String, String>,
    root: &std::path::Path,
) -> Vec<String> {
    let mut universe: Vec<String> = cells
        .iter()
        .filter(|(_, path)| root.join(cell_path_suffix(path, root)).is_dir())
        .map(|(cell, _)| format!("{cell}//..."))
        .collect();
    universe.sort();
    universe.dedup();
    universe
}

/// Whether a cell path from `audit cell` (absolute) lies inside the
/// project root, as a root-relative suffix for the existence check.
fn cell_path_suffix(path: &str, root: &std::path::Path) -> PathBuf {
    match std::path::Path::new(path).strip_prefix(root) {
        Ok(relative) => relative.to_path_buf(),
        // A cell outside the checkout (e.g. a sibling third-party
        // checkout) still counts when it exists on disk.
        Err(_) => PathBuf::from(path),
    }
}

/// Qualify project-relative files with their owning cell (`cell//path`).
///
/// The BXL's `cquery.owner()` resolves relative paths against the BXL
/// script's own cell, so in a consumer repo (where the script lives in
/// the external `buckshot` cell) every raw path maps into the buckshot
/// cell and finds no owner. Qualified labels resolve correctly from any
/// cell. Ownership follows the longest cell-path prefix, so nested cells
/// win; files outside every cell keep their raw form.
fn qualify_files(
    files: &[String],
    cells: &std::collections::BTreeMap<String, String>,
    root: &std::path::Path,
) -> Vec<String> {
    files
        .iter()
        .map(|file| {
            let abs = root.join(file);
            let mut best: Option<(&String, &String)> = None;
            for (cell, path) in cells {
                let cell_path = std::path::Path::new(path);
                if abs.starts_with(cell_path)
                    && best
                        .as_ref()
                        .is_none_or(|(_, best_path)| best_path.len() < path.len())
                {
                    best = Some((cell, path));
                }
            }
            match best {
                Some((cell, path)) => match abs.strip_prefix(path) {
                    Ok(rel) if !rel.as_os_str().is_empty() => {
                        let rel = rel.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
                        format!("{cell}//{rel}")
                    }
                    _ => file.clone(),
                },
                None => file.clone(),
            }
        })
        .collect()
}

/// A target pattern for `--only` filtering: an optional cell plus a path
/// that is either recursive (`...`), a whole package (empty name), or one
/// named target.
#[derive(Debug, PartialEq, Eq)]
struct TargetPattern {
    cell: Option<String>,
    path: String,
    name: Option<String>,
    recursive: bool,
}

impl TargetPattern {
    fn parse(pattern: &str) -> anyhow::Result<Self> {
        let (cell, rest) = match pattern.split_once("//") {
            Some((cell, rest)) => {
                let cell = if cell.is_empty() {
                    None
                } else {
                    Some(cell.to_string())
                };
                (cell, rest)
            }
            None => anyhow::bail!("invalid target pattern `{pattern}` (expected `cell//...`, `cell//path/...`, `cell//path:name`, or `cell//path:`)"),
        };
        if rest == "..." || rest.is_empty() {
            return Ok(Self {
                cell,
                path: String::new(),
                name: None,
                recursive: true,
            });
        }
        if let Some(path) = rest.strip_suffix("/...") {
            return Ok(Self {
                cell,
                path: path.to_string(),
                name: None,
                recursive: true,
            });
        }
        if let Some((path, name)) = rest.split_once(':') {
            return Ok(Self {
                cell,
                path: path.to_string(),
                name: if name.is_empty() { None } else { Some(name.to_string()) },
                recursive: false,
            });
        }
        anyhow::bail!("invalid target pattern `{pattern}` (expected `cell//...`, `cell//path/...`, `cell//path:name`, or `cell//path:`)")
    }

    /// Whether an unconfigured `cell//path:name` label matches. A pattern
    /// without a cell matches in any cell (over-matching just runs more
    /// tests; under-matching would silently skip them).
    fn matches(&self, target: &str) -> bool {
        let (cell, rest) = match target.split_once("//") {
            Some((cell, rest)) => (cell, rest),
            None => return false,
        };
        if self.cell.as_deref().is_some_and(|c| c != cell) {
            return false;
        }
        let (path, _name) = rest.split_once(':').unwrap_or((rest, ""));
        if self.recursive {
            return self.path.is_empty() || path == self.path || path.starts_with(&format!("{}/", self.path));
        }
        if path != self.path {
            return false;
        }
        match &self.name {
            None => true,
            Some(name) => rest.split_once(':').is_some_and(|(_, n)| n == name),
        }
    }
}

/// Directory all SCM and buck2 invocations run in: the checkout root, so
/// relative file paths line up with buck cell paths whatever the caller's
/// cwd is.
fn project_root(scm: Scm) -> anyhow::Result<PathBuf> {
    // Probe Sapling first only when it could plausibly own the checkout;
    // otherwise `git` is the answer (and the error it reports on failure
    // names the real problem).
    if scm != Scm::Git && command_succeeds("sl", &["root"]) {
        return scm_root("sl");
    }
    if scm == Scm::Sl {
        anyhow::bail!("`--scm sl` was requested but `sl root` failed here");
    }
    scm_root("git")
}

fn scm_root(tool: &str) -> anyhow::Result<PathBuf> {
    let flag = if tool == "sl" { "root" } else { "rev-parse --show-toplevel" };
    let args: Vec<&str> = flag.split(' ').collect();
    let output = Command::new(tool)
        .args(&args)
        .output()
        .with_context(|| format!("running `{tool} {flag}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`{tool} {flag}` failed:\n{stderr}");
    }
    let root = String::from_utf8(output.stdout).context("non-UTF-8 output")?;
    Ok(PathBuf::from(root.trim()))
}

fn detect_scm(root: &std::path::Path) -> ResolvedScm {
    // `.sl` beside the root marks a Sapling checkout; `.git` (dir or
    // worktree file) marks git. Prefer Sapling when both somehow exist,
    // matching `sl`'s own ownership of such checkouts.
    if root.join(".sl").exists() {
        ResolvedScm::Sl
    } else {
        ResolvedScm::Git
    }
}

fn command_succeeds(tool: &str, args: &[&str]) -> bool {
    Command::new(tool)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn changed_files(
    scm: ResolvedScm,
    root: &std::path::Path,
    base: &str,
    head: &str,
    merge_base: bool,
    uncommitted: bool,
    all_files: bool,
) -> anyhow::Result<Vec<String>> {
    let mut files = match scm {
        ResolvedScm::Git => git_changed_files(root, base, head, merge_base, uncommitted, all_files)?,
        ResolvedScm::Sl => sl_changed_files(root, base, head, merge_base, uncommitted, all_files)?,
    };
    files.sort();
    files.dedup();
    Ok(files)
}

/// Caller-supplied file list, one path per line (e.g. from the GitHub
/// pull-request files API). Blank lines are ignored.
fn read_files_list(path: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading changed-file list from {}", path.display()))?;
    Ok(parse_line_separated(&body))
}

/// Uncommitted working-copy changes: tracked files differing from HEAD
/// plus new (non-ignored) files under git, `sl status` output under
/// Sapling.
fn uncommitted_files(scm: ResolvedScm, root: &std::path::Path) -> anyhow::Result<Vec<String>> {
    match scm {
        ResolvedScm::Git => {
            // Tracked changes versus HEAD, covering staged and unstaged
            // edits in one invocation ...
            let mut files = run_git_diff(root, &["diff", "--name-only", "-z", "HEAD", "--"])?;
            // ... plus brand-new files git ignores nothing about
            // (`--exclude-standard` keeps build output and editor junk
            // out). A new file covered by a target's `glob()` resolves an
            // owner like any other file; one owned by nothing (docs, CI
            // config) just warns inside the BXL query.
            files.extend(run_git_files(root, &["ls-files", "--others", "--exclude-standard", "-z"])?);
            Ok(files)
        }
        ResolvedScm::Sl => run_sl_status(root, &[]),
    }
}

fn git_changed_files(
    root: &std::path::Path,
    base: &str,
    head: &str,
    merge_base: bool,
    uncommitted: bool,
    all_files: bool,
) -> anyhow::Result<Vec<String>> {
    let mut files = if all_files {
        // Every tracked file counts: no range, no merge-base, no magic
        // empty-tree hash.
        run_git_files(root, &["ls-files", "--cached", "-z"])?
    } else {
        let start = if merge_base {
            git_merge_base(root, base, head)?
        } else {
            base.to_string()
        };
        run_git_diff(root, &["diff", "--name-only", "-z", &start, head, "--"])?
    };
    if uncommitted {
        files.extend(uncommitted_files(ResolvedScm::Git, root)?);
    }
    Ok(files)
}

fn git_merge_base(root: &std::path::Path, base: &str, head: &str) -> anyhow::Result<String> {
    let output = Command::new("git")
        .current_dir(root)
        .args(["merge-base", head, base])
        .output()
        .context("running `git merge-base`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`git merge-base {head} {base}` failed (shallow clone without the base? CI needs `fetch-depth: 0`):\n{stderr}");
    }
    Ok(String::from_utf8(output.stdout)
        .context("non-UTF-8 `git merge-base` output")?
        .trim()
        .to_string())
}

fn run_git_diff(root: &std::path::Path, args: &[&str]) -> anyhow::Result<Vec<String>> {
    run_git_files(root, args)
}

fn run_git_files(root: &std::path::Path, args: &[&str]) -> anyhow::Result<Vec<String>> {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`git {}` failed:\n{stderr}", args.join(" "));
    }
    Ok(parse_nul_separated(&output.stdout))
}

/// `sl` spellings of the same range query. Sapling revsets address the
/// working copy as `.`, so pass `--head .` (and a bookmark like
/// `remote/main` for `--base`) in Sapling checkouts.
fn sl_changed_files(
    root: &std::path::Path,
    base: &str,
    head: &str,
    merge_base: bool,
    uncommitted: bool,
    all_files: bool,
) -> anyhow::Result<Vec<String>> {
    let mut files = if all_files {
        // Every tracked file counts, no range involved.
        run_sl_files(root)?
    } else {
        let start = if merge_base {
            sl_merge_base(root, base, head)?
        } else {
            base.to_string()
        };
        run_sl_status(root, &[&start, head])?
    };
    if uncommitted {
        files.extend(uncommitted_files(ResolvedScm::Sl, root)?);
    }
    Ok(files)
}

/// All files tracked in the working copy.
fn run_sl_files(root: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let output = Command::new("sl")
        .current_dir(root)
        .args(["files", "-T", "{path}\n"])
        .output()
        .context("running `sl files`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`sl files` failed:\n{stderr}");
    }
    Ok(parse_line_separated(&String::from_utf8(output.stdout).context("non-UTF-8 `sl files` output")?))
}

fn sl_merge_base(root: &std::path::Path, base: &str, head: &str) -> anyhow::Result<String> {
    let revset = ancestor_revset(head, base);
    let output = Command::new("sl")
        .current_dir(root)
        .args(["log", "-r", &revset, "-T", "{node}\n"])
        .output()
        .context("running `sl log` for the merge-base")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`sl log -r {revset}` failed:\n{stderr}");
    }
    let node = String::from_utf8(output.stdout)
        .context("non-UTF-8 `sl log` output")?
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string();
    if node.is_empty() {
        anyhow::bail!("`sl log -r {revset}` printed no commit (do `{head}` and `{base}` share history?)");
    }
    Ok(node)
}

/// `sl status` between two revisions lists exactly the files that differ;
/// with no revisions it lists uncommitted working-copy changes.
fn run_sl_status(root: &std::path::Path, revs: &[&str]) -> anyhow::Result<Vec<String>> {
    let mut args = vec!["status", "--no-status"];
    for rev in revs {
        args.push("--rev");
        args.push(rev);
    }
    let output = Command::new("sl")
        .current_dir(root)
        .args(&args)
        .output()
        .with_context(|| format!("running `sl {}`", args.join(" ")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`sl {}` failed:\n{stderr}", args.join(" "));
    }
    Ok(parse_line_separated(&String::from_utf8(output.stdout).context("non-UTF-8 `sl status` output")?))
}

fn ancestor_revset(head: &str, base: &str) -> String {
    format!("ancestor({head}, {base})")
}

/// `git diff -z` separates paths with NUL and ends the output with one;
/// an empty diff is a single empty string, not a file.
fn parse_nul_separated(output: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(output);
    text.split('\0')
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect()
}

fn parse_line_separated(output: &str) -> Vec<String> {
    output
        .lines()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .collect()
}

/// Runs the owner + rdeps BXL query and returns the affected target
/// labels, sorted and deduplicated.
fn query_affected(
    buck2: &str,
    bxl: &str,
    root: &std::path::Path,
    files: &[String],
    universe: &[String],
) -> anyhow::Result<Vec<String>> {
    let mut command = Command::new(buck2);
    command.current_dir(root).args(bxl_argv(bxl, files, universe));
    let output = command
        .stdout(Stdio::piped())
        .output()
        .with_context(|| format!("running `{buck2} bxl {bxl}`"))?;
    if !output.status.success() {
        // BXL progress and query warnings go to stderr, which is
        // inherited below on failure for diagnosability.
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`{buck2} bxl {bxl}` failed:\n{stderr}");
    }
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    Ok(parse_target_list(&String::from_utf8(output.stdout).context("non-UTF-8 BXL output")?))
}

/// `buck2 bxl <label> -- --files <f...> --universe <u...>`: the BXL
/// script declares both as `cli_args.list(cli_args.string())`, which takes
/// space-separated values after each flag.
fn bxl_argv<'a>(bxl: &'a str, files: &'a [String], universe: &'a [String]) -> Vec<&'a str> {
    let mut argv = vec!["bxl", bxl, "--", "--files"];
    argv.extend(files.iter().map(String::as_str));
    argv.push("--universe");
    argv.extend(universe.iter().map(String::as_str));
    argv
}

/// Keep affected targets inside the universe, narrowed further by the
/// `--only` patterns (empty `only` means no further narrowing). Sorted
/// and deduplicated for stable output.
fn filter_targets(
    targets: Vec<String>,
    universe: &[TargetPattern],
    only: &[TargetPattern],
) -> Vec<String> {
    let mut kept: Vec<String> = targets
        .into_iter()
        .filter(|target| {
            universe.iter().any(|pattern| pattern.matches(target))
                && (only.is_empty() || only.iter().any(|pattern| pattern.matches(target)))
        })
        .collect();
    kept.sort();
    kept.dedup();
    kept
}

/// One label per line; blank lines (e.g. a trailing newline) carry no
/// target. Sorted and deduplicated so output is stable regardless of
/// query order.
fn parse_target_list(output: &str) -> Vec<String> {
    let mut targets = parse_line_separated(output);
    targets.sort();
    targets.dedup();
    targets
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[derive(clap::Parser)]
    struct TestCli {
        #[command(flatten)]
        args: ChangedTargetsArgs,
    }

    #[test]
    fn defaults_target_main_with_auto_universe() {
        let cli = TestCli::try_parse_from(["changed-targets"]).unwrap();
        assert_eq!(cli.args.base, "origin/main");
        assert_eq!(cli.args.head, "HEAD");
        assert!(!cli.args.no_merge_base);
        assert!(!cli.args.uncommitted);
        assert!(!cli.args.all_files);
        assert_eq!(cli.args.files_from, None);
        // No `--universe` means auto-discovery (every local cell), and no
        // `--only` means no output filtering.
        assert!(cli.args.universe.is_empty());
        assert!(cli.args.only.is_empty());
        assert_eq!(cli.args.buck2, "buck2");
        assert_eq!(
            cli.args.bxl,
            "buckshot//scripts/affected_targets.bxl:affected_targets"
        );
    }

    #[test]
    fn all_files_and_only_are_repeatable() {
        let cli = TestCli::try_parse_from([
            "changed-targets",
            "--all-files",
            "--only",
            "tests//...",
            "--only",
            "buckshot//buckshot:buckshot",
        ])
        .unwrap();
        assert!(cli.args.all_files);
        assert_eq!(cli.args.only, vec!["tests//...", "buckshot//buckshot:buckshot"]);
    }

    #[test]
    fn universe_from_cells_skips_nonexistent_paths() {
        let root = std::env::temp_dir().join(format!("ci-cells-test-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        let cells = std::collections::BTreeMap::from([
            ("buckshot".to_string(), root.to_string_lossy().into_owned()),
            (
                "tests".to_string(),
                root.join("tests").to_string_lossy().into_owned(),
            ),
            (
                "prelude".to_string(),
                root.join("prelude").to_string_lossy().into_owned(),
            ),
        ]);
        assert_eq!(
            universe_from_cells(&cells, &root),
            vec!["buckshot//...".to_string(), "tests//...".to_string()]
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn cell_path_suffix_handles_inside_outside_and_root() {
        let root = std::path::Path::new("/repo");
        assert_eq!(
            cell_path_suffix("/repo/tests", root),
            std::path::PathBuf::from("tests")
        );
        assert_eq!(cell_path_suffix("/repo", root), PathBuf::from(""));
        assert_eq!(
            cell_path_suffix("/elsewhere/lib", root),
            std::path::PathBuf::from("/elsewhere/lib")
        );
    }

    #[test]
    fn qualify_files_prefers_longest_cell_prefix() {
        let root = std::path::Path::new("/repo");
        let cells = std::collections::BTreeMap::from([
            ("root".to_string(), "/repo".to_string()),
            ("macros".to_string(), "/repo/macros".to_string()),
            ("third-party".to_string(), "/repo/third-party".to_string()),
            // Trailing slashes must not break the prefix match.
            ("toolchains".to_string(), "/repo/toolchains/".to_string()),
            ("tests".to_string(), "/repo/tests".to_string()),
        ]);
        let files = [
            "robot/main.rs",
            "macros/rust.bzl",
            "third-party/rust/BUCK",
            "toolchains/opensdk/BUCK",
            // Already-qualified input round-trips unchanged.
            "tests//rust/main.rs",
        ]
        .iter()
        .map(|file| file.to_string())
        .collect::<Vec<_>>();
        assert_eq!(
            qualify_files(&files, &cells, root),
            vec![
                "root//robot/main.rs".to_string(),
                "macros//rust.bzl".to_string(),
                "third-party//rust/BUCK".to_string(),
                "toolchains//opensdk/BUCK".to_string(),
                "tests//rust/main.rs".to_string(),
            ]
        );
    }

    #[test]
    fn qualify_files_leaves_outside_files_raw() {
        let root = std::path::Path::new("/repo");
        // No cell covers this file: the raw path flows through exactly as
        // before, and the BXL resolves it as it always has.
        let cells = std::collections::BTreeMap::from([(
            "elsewhere".to_string(),
            "/elsewhere/lib".to_string(),
        )]);
        let files = ["lonely.rs".to_string()];
        assert_eq!(qualify_files(&files, &cells, root), files);
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        for bad in ["no-cells-here", "cell//path", "cell//path/.../extra"] {
            assert!(TargetPattern::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn patterns_match_labels() {
        let target = "tests//rust:hello-unittest";
        // Recursive, package, and exact forms.
        assert!(TargetPattern::parse("tests//...").unwrap().matches(target));
        assert!(TargetPattern::parse("tests//rust/...").unwrap().matches(target));
        assert!(TargetPattern::parse("tests//rust:").unwrap().matches(target));
        assert!(TargetPattern::parse("tests//rust:hello-unittest")
            .unwrap()
            .matches(target));
        // `rust/...` must not swallow a `rusty/...` sibling.
        assert!(!TargetPattern::parse("tests//rust/...").unwrap().matches("tests//rusty:x"));
        assert!(TargetPattern::parse("tests//rust/...").unwrap().matches("tests//rust/nested:x"));
        // Wrong cell, wrong package, wrong name.
        assert!(!TargetPattern::parse("buckshot//...").unwrap().matches(target));
        assert!(!TargetPattern::parse("tests//python/...").unwrap().matches(target));
        assert!(!TargetPattern::parse("tests//rust:hello").unwrap().matches(target));
        assert!(!TargetPattern::parse("tests//rust:hello-unittest")
            .unwrap()
            .matches("tests//rust:hello"));
        // Cell-less patterns match in any cell.
        assert!(TargetPattern::parse("//...").unwrap().matches(target));
        assert!(TargetPattern::parse("//rust/...").unwrap().matches(target));
        assert!(!TargetPattern::parse("//python/...").unwrap().matches(target));
        // Malformed labels never match.
        assert!(!TargetPattern::parse("tests//...").unwrap().matches("not-a-label"));
    }

    #[test]
    fn universe_is_repeatable_and_base_is_overridable() {
        let cli = TestCli::try_parse_from([
            "changed-targets",
            "--base",
            "abc123",
            "--no-merge-base",
            "--universe",
            "buckshot//...",
            "--universe",
            "tests//...",
        ])
        .unwrap();
        assert_eq!(cli.args.base, "abc123");
        assert!(cli.args.no_merge_base);
        assert_eq!(cli.args.universe, vec!["buckshot//...", "tests//..."]);
    }

    #[test]
    fn nul_parsing_ignores_trailing_nul_and_blanks() {
        assert_eq!(
            parse_nul_separated(b"a/b.rs\0c d/e.rs\0\0"),
            vec!["a/b.rs".to_string(), "c d/e.rs".to_string()]
        );
        assert!(parse_nul_separated(b"").is_empty());
        assert!(parse_nul_separated(b"\0").is_empty());
    }

    #[test]
    fn line_parsing_trims_and_skips_blanks() {
        assert_eq!(
            parse_line_separated("a/b.rs\n\n  c/e.rs  \n"),
            vec!["a/b.rs".to_string(), "c/e.rs".to_string()]
        );
        assert!(parse_line_separated("").is_empty());
    }

    #[test]
    fn filter_targets_scopes_to_universe_then_only() {
        let targets = vec![
            "buckshot//cxx/toolchain:toolchain".to_string(),
            "tests//rust:hello".to_string(),
            "tests//python:hello-test".to_string(),
        ];
        let universe = vec![TargetPattern::parse("tests//...").unwrap()];
        // Universe alone drops the out-of-scope cxx target but keeps
        // in-scope reverse dependencies no matter who owns them.
        assert_eq!(
            filter_targets(targets.clone(), &universe, &[]),
            vec!["tests//python:hello-test".to_string(), "tests//rust:hello".to_string()]
        );
        // `--only` intersects further.
        let only = vec![TargetPattern::parse("tests//rust/...").unwrap()];
        assert_eq!(
            filter_targets(targets.clone(), &universe, &only),
            vec!["tests//rust:hello".to_string()]
        );
        // Nothing in scope means nothing printed (exit stays 0).
        let only = vec![TargetPattern::parse("tests//wasm/...").unwrap()];
        assert!(filter_targets(targets, &universe, &only).is_empty());
    }

    #[test]
    fn target_list_is_sorted_deduplicated_and_blank_free() {
        assert_eq!(
            parse_target_list("b//:t2\na//:t1\nb//:t2\n\n"),
            vec!["a//:t1".to_string(), "b//:t2".to_string()]
        );
    }

    #[test]
    fn bxl_argv_lists_files_then_universe() {
        let files = vec!["a.rs".to_string(), "b/c.rs".to_string()];
        let universe = vec!["buckshot//...".to_string(), "tests//...".to_string()];
        assert_eq!(
            bxl_argv("my//script:fn", &files, &universe),
            vec![
                "bxl",
                "my//script:fn",
                "--",
                "--files",
                "a.rs",
                "b/c.rs",
                "--universe",
                "buckshot//...",
                "tests//...",
            ]
        );
    }

    #[test]
    fn files_from_is_parsed_as_path() {
        let cli = TestCli::try_parse_from(["changed-targets", "--files-from", "files.txt"]).unwrap();
        assert_eq!(cli.args.files_from, Some(PathBuf::from("files.txt")));
    }

    #[test]
    fn files_list_read_ignores_blank_lines() {
        let dir = std::env::temp_dir().join(format!("ci-files-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("files.txt");
        std::fs::write(&path, "a/b.rs\n\n  c/d.rs  \n").unwrap();
        assert_eq!(
            read_files_list(&path).unwrap(),
            vec!["a/b.rs".to_string(), "c/d.rs".to_string()]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn files_list_read_missing_file_is_an_error() {
        let err = read_files_list(std::path::Path::new("/nonexistent-ci-files-list.txt")).unwrap_err();
        assert!(err.to_string().contains("reading changed-file list"), "{err}");
    }

    #[test]
    fn buck2_resolution_anchors_relative_paths_at_root() {
        let root = std::path::Path::new("/repo");
        // Bare names go through PATH lookup untouched.
        assert_eq!(resolve_buck2(root, "buck2"), "buck2");
        // Absolute paths pass through untouched.
        assert_eq!(resolve_buck2(root, "/usr/local/bin/buck2"), "/usr/local/bin/buck2");
        // Root-relative wrapper paths anchor at the project root, so they
        // work whatever the caller's cwd is.
        assert_eq!(resolve_buck2(root, "./buck2"), "/repo/./buck2");
        assert_eq!(resolve_buck2(root, "tools/buck2"), "/repo/tools/buck2");
    }

    #[test]
    fn sl_merge_base_uses_ancestor_revset() {
        assert_eq!(ancestor_revset(".", "remote/main"), "ancestor(., remote/main)");
    }

    #[test]
    fn detect_scm_prefers_sapling_marker() {
        let dir = std::env::temp_dir().join(format!("ci-scm-test-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join(".sl")).unwrap();
        assert_eq!(detect_scm(&dir), ResolvedScm::Sl);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(detect_scm(&dir), ResolvedScm::Git);
        std::fs::remove_dir_all(&dir).ok();
    }
}
