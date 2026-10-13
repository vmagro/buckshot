//! Generate rust-analyzer's `rust-project.json` for buck targets.
//!
//! Wraps the upstream `rust-project` tool's `develop` subcommand: builds the
//! given toolchain's `sysroot-with-src` subtarget, runs `develop --sysroot`
//! against it, then fills in the `sysroot_src` field `develop --sysroot`
//! cannot emit itself (it hardcodes `None`; its only discovery mechanism is
//! the Meta-monorepo-only `rust.sysroot_src_path` buckconfig key, queried
//! via `buck2 audit config`). Without `sysroot_src` rust-analyzer loads an
//! empty sysroot and reports bogus diagnostics on everything std-dependent
//! (e.g. `cannot apply unary operator '-' to type 'f64'`).
//!
//! Run from a consumer repo:
//!
//! ```sh
//! buck2 run buckshot//buckshot:buckshot -- rust rust-project \
//!     --toolchain toolchains//:rust //my/targets/...
//! ```
//!
//! This writes `./rust-project.json` (wherever it runs, i.e. the current
//! directory), pointing `sysroot`/`sysroot_src` at the buck-built sysroot.
//! No rustup involvement: the IDE stays on the buck-defined toolchain.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Args;

#[derive(Args)]
pub struct RustProjectArgs {
    /// Toolchain target whose `sysroot-with-src` subtarget provides the
    /// sysroot (e.g. `toolchains//:rust`).
    #[arg(long)]
    toolchain: String,

    /// `rust-project` executable target (upstream dotslash pin).
    #[arg(long, default_value = "buckshot//tools/buck:rust-project")]
    rust_project_target: String,

    /// Buck target patterns to include in `rust-project.json`
    /// (e.g. `//habit/...`).
    #[arg(required = true)]
    targets: Vec<String>,
}

pub fn rust_project(args: RustProjectArgs) -> anyhow::Result<()> {
    let sysroot = build_sysroot(&args.toolchain)?;
    run_develop(&args.rust_project_target, &sysroot, &args.targets)?;
    // `develop` writes `./rust-project.json` in the current directory.
    let json_path = PathBuf::from("rust-project.json");
    let src = set_sysroot_src_file(&json_path, &sysroot)?;
    println!("sysroot: {}", sysroot.display());
    println!("sysroot_src: {}", src.display());
    Ok(())
}

/// Build `<toolchain>[sysroot-with-src]` and return its absolute path.
fn build_sysroot(toolchain: &str) -> anyhow::Result<PathBuf> {
    let target = format!("{toolchain}[sysroot-with-src]");
    let output = std::process::Command::new("buck2")
        .args(["build", "--show-simple-output"])
        .arg(&target)
        .output()
        .with_context(|| format!("building {target}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("`buck2 build {target}` failed:\n{stderr}");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first = parse_simple_output(&stdout)
        .with_context(|| format!("parsing `buck2 build {target}` output"))?;
    // `--show-simple-output` prints project-relative paths; absolutize
    // against the current directory (a no-op when already absolute).
    let abs = std::env::current_dir()
        .context("resolving current directory")?
        .join(first);
    abs.canonicalize()
        .with_context(|| format!("resolving built sysroot at {}", abs.display()))
}

/// The single path in `buck2 build --show-simple-output` output.
fn parse_simple_output(out: &str) -> anyhow::Result<&str> {
    let mut lines = out.lines().filter(|l| !l.trim().is_empty());
    let first = lines
        .next()
        .context("`buck2 build --show-simple-output` printed no output")?;
    if lines.next().is_some() {
        anyhow::bail!("expected a single output path, got several:\n{out}");
    }
    Ok(first)
}

/// Run `develop --sysroot <sysroot> <targets>`, inheriting stdio so the
/// tool's progress stays visible. Writes `./rust-project.json`.
fn run_develop(target: &str, sysroot: &Path, targets: &[String]) -> anyhow::Result<()> {
    let status = std::process::Command::new("buck2")
        .arg("run")
        .arg(target)
        .arg("--")
        .arg("develop")
        .arg("--sysroot")
        .arg(sysroot)
        .args(targets)
        .status()
        .with_context(|| format!("running {target}"))?;
    if !status.success() {
        anyhow::bail!("`buck2 run {target}` failed");
    }
    Ok(())
}

/// `lib/rustlib/src/rust/library` under the sysroot: the std sources
/// rust-analyzer loads.
fn sysroot_src_dir(sysroot: &Path) -> PathBuf {
    sysroot.join("lib/rustlib/src/rust/library")
}

/// Fill in `sysroot_src` in the generated JSON file. Returns the src dir.
fn set_sysroot_src_file(json_path: &Path, sysroot: &Path) -> anyhow::Result<PathBuf> {
    let bytes =
        std::fs::read(json_path).with_context(|| format!("reading {}", json_path.display()))?;
    let mut json: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", json_path.display()))?;
    let src = set_sysroot_src(&mut json, sysroot)?;
    // Match `develop`'s own format: compact JSON plus a trailing newline
    // (key order becomes alphabetical; JSON semantics are unaffected).
    let mut rendered = serde_json::to_string(&json).context("rendering rust-project.json")?;
    rendered.push('\n');
    std::fs::write(json_path, rendered)
        .with_context(|| format!("writing {}", json_path.display()))?;
    Ok(src)
}

/// Set `sysroot_src` on the parsed `develop` output, verifying the sysroot
/// the tool recorded matches the one we built and that it ships rust-src.
fn set_sysroot_src(json: &mut serde_json::Value, sysroot: &Path) -> anyhow::Result<PathBuf> {
    let recorded = json
        .get("sysroot")
        .and_then(serde_json::Value::as_str)
        .context("generated rust-project.json has no `sysroot` field")?;
    let same = std::fs::canonicalize(Path::new(recorded))
        .ok()
        .zip(std::fs::canonicalize(sysroot).ok())
        .is_some_and(|(a, b)| a == b);
    if !same {
        anyhow::bail!(
            "rust-project recorded sysroot `{recorded}` but the built sysroot is `{}`",
            sysroot.display()
        );
    }
    let src = sysroot_src_dir(sysroot);
    if !src.join("core/src/lib.rs").is_file() {
        anyhow::bail!(
            "built sysroot has no std sources at {} (toolchain predates rust-src?)",
            src.display()
        );
    }
    json.as_object_mut()
        .context("generated rust-project.json is not an object")?
        .insert(
            "sysroot_src".to_string(),
            serde_json::Value::String(src.to_string_lossy().into_owned()),
        );
    Ok(src)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scratch sysroot of the given shape: `with_src` controls whether
    /// `lib/rustlib/src/rust/library/core/src/lib.rs` exists.
    fn scratch_sysroot(name: &str, with_src: bool) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("rust-project-test-{}-{name}.d", std::process::id()));
        // Start clean: a stale dir from a crashed run must not leak state
        // (pid reuse) into the shape under test.
        std::fs::remove_dir_all(&root).ok();
        if with_src {
            let core = root.join("lib/rustlib/src/rust/library/core/src");
            std::fs::create_dir_all(&core).unwrap();
            std::fs::write(core.join("lib.rs"), b"").unwrap();
        } else {
            std::fs::create_dir_all(&root).unwrap();
        }
        root
    }

    #[test]
    fn sets_sysroot_src() {
        let root = scratch_sysroot("sets-src", true);
        let mut json = serde_json::json!({"sysroot": root.to_string_lossy(), "crates": []});
        let src = set_sysroot_src(&mut json, &root).unwrap();
        assert_eq!(src, root.join("lib/rustlib/src/rust/library"));
        assert_eq!(
            json["sysroot_src"],
            serde_json::json!(src.to_string_lossy())
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn missing_rust_src_is_an_error() {
        let root = scratch_sysroot("no-src", false);
        let mut json = serde_json::json!({"sysroot": root.to_string_lossy()});
        let err = set_sysroot_src(&mut json, &root).unwrap_err();
        assert!(err.to_string().contains("no std sources"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn sysroot_mismatch_is_an_error() {
        let root = scratch_sysroot("mismatch", true);
        let mut json = serde_json::json!({"sysroot": "/somewhere/else"});
        let err = set_sysroot_src(&mut json, &root).unwrap_err();
        assert!(err.to_string().contains("recorded sysroot"), "{err}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn parses_single_build_output_line() {
        assert_eq!(
            parse_simple_output("buck-out/foo\n").unwrap(),
            "buck-out/foo"
        );
    }

    #[test]
    fn rejects_empty_or_multiple_build_output_lines() {
        assert!(parse_simple_output("\n  \n").is_err());
        assert!(parse_simple_output("a\nb\n").is_err());
    }
}
