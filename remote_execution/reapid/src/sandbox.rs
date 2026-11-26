//! Action sandboxing through the `nono` crate (Landlock on Linux, Seatbelt
//! on macOS).
//!
//! Every executed action runs under deny-by-default confinement: only the
//! action's declared working set is reachable — the staged exec root
//! (inputs and outputs, read/write), the program binary plus its script
//! interpreter (read-only), and the dynamic loader with its libraries
//! (read-only). System tool directories are not granted; toolchains are
//! hermetic. Networking is always blocked. Anything the action spawns
//! itself (a shell's `$(tool)` lookup, an undeclared config read) must
//! live inside the staged inputs or it fails closed.
//!
//! The split mirrors process lifetime: [`prepare`] builds everything in
//! the daemon (parent) ahead of time — the [`nono::CapabilitySet`],
//! Landlock ABI detection, and the pre-opened path rules — and the child
//! applies it in `pre_exec`, after `fork` and before `exec`.
//! Enforcement must happen in the child: sandboxing is irreversible and
//! would otherwise confine the daemon itself.
//!
//! Pre-exec safety is why preparation is split this way. `pre_exec` runs
//! in a forked child of a multithreaded runtime, where only
//! async-signal-safe work is sound. On Linux the prepared sandbox applies
//! with raw syscalls only ([`nono::sandbox::PreparedLandlockSandbox::apply_raw`]
//! neither allocates nor touches parent state), so the child does no
//! allocation at all. The macOS path builds its profile in the child;
//! that allocation is inherent to Seatbelt setup.
//!
//! When the platform cannot sandbox (`Sandbox::is_supported()` is false),
//! preparation returns `None` and the action runs unsandboxed with a
//! warning. Enforcement failures in the child fail the spawn instead:
//! a sandbox that cannot be entered must never silently run open.

use std::io;
use std::path::Path;
use std::path::PathBuf;

/// A sandbox prepared ahead of time, ready to apply in `pre_exec`.
pub struct PreparedSandbox {
    /// Pre-opened Landlock ruleset; applied with raw syscalls only.
    #[cfg(target_os = "linux")]
    prepared: std::sync::OnceLock<nono::sandbox::PreparedLandlockSandbox>,
    /// Seatbelt profile source; applied in the child.
    #[cfg(target_os = "macos")]
    caps: nono::CapabilitySet,
}

/// Prepare the sandbox for one action ahead of spawning.
///
/// `exec_root` is the staged input root (every declared input and output
/// lives under it); `program` is the resolved executable. Returns `None`
/// on platforms without sandbox support (the action then runs
/// unsandboxed; see module docs).
#[cfg(unix)]
pub fn prepare(exec_root: &Path, program: &Path) -> Result<Option<PreparedSandbox>, String> {
    if !nono::Sandbox::is_supported() {
        let info = nono::Sandbox::support_info();
        tracing::warn!(
            platform = info.platform,
            details = %info.details,
            "sandboxing unsupported on this host; executing without confinement",
        );
        return Ok(None);
    }
    let caps = build_capabilities(exec_root, program)?;
    #[cfg(target_os = "linux")]
    {
        let abi = nono::Sandbox::detect_abi()
            .map_err(|e| format!("sandbox ABI detection failed: {e}"))?;
        let prepared =
            nono::Sandbox::prepare_seccomp_with_abi(&caps, &abi, nono::sandbox::SeccompOpts::network_fallback())
                .map_err(|e| format!("sandbox prepare failed: {e}"))?;
        // ProxyOnly would need a supervisor listener the child cannot
        // serve; with a blocked network the fallback must be inline.
        if !matches!(
            prepared.fallback(),
            nono::sandbox::SeccompNetFallback::None
                | nono::sandbox::SeccompNetFallback::BlockAll
        ) {
            return Err("sandbox prepared a proxy fallback; refusing to run".to_string());
        }
        let slot = std::sync::OnceLock::new();
        if slot.set(prepared).is_err() {
            return Err("sandbox already prepared".to_string());
        }
        Ok(Some(PreparedSandbox { prepared: slot }))
    }
    #[cfg(target_os = "macos")]
    {
        Ok(Some(PreparedSandbox { caps }))
    }
}

/// No-op on platforms without sandbox support (notably Windows, where
/// the nono dependency is excluded from the build): actions run
/// unsandboxed, exactly as if `is_supported()` had returned false.
#[cfg(not(unix))]
pub fn prepare(_exec_root: &Path, _program: &Path) -> Result<Option<PreparedSandbox>, String> {
    Ok(None)
}

/// Apply a prepared sandbox. Call only in the `pre_exec` child, after
/// `fork` and before `exec`.
#[cfg_attr(not(unix), allow(unused_variables))]
pub fn apply_in_child(sandbox: &PreparedSandbox) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let prepared = sandbox
            .prepared
            .get()
            .ok_or_else(|| io::Error::other("sandbox was not prepared"))?;
        // Raw syscalls only: no allocation, no parent state. The errno
        // survives without formatting so this stays allocation-free.
        prepared
            .apply_raw()
            .map_err(|e| io::Error::from_raw_os_error(e.errno()))
    }
    #[cfg(target_os = "macos")]
    {
        nono::Sandbox::apply_auto(&sandbox.caps).map_err(|e| io::Error::other(e.to_string()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Ok(())
    }
}

/// Build the capability set: the working set plus the minimal system
/// paths needed to launch anything. Best-effort throughout except the
/// exec root — a missing system path is skipped (fail closed at exec),
/// but a missing exec root is an internal error.
#[cfg(unix)]
fn build_capabilities(exec_root: &Path, program: &Path) -> Result<nono::CapabilitySet, String> {
    let mut caps = nono::CapabilitySet::new();
    // The working set: staged inputs are read, declared outputs and
    // action temp files (see TMPDIR below) are written here.
    caps = caps
        .allow_path(exec_root, nono::AccessMode::ReadWrite)
        .map_err(|e| format!("sandbox exec root {}: {e}", exec_root.display()))?;
    // The program itself when it lives outside the staged tree (an
    // absolute path or a daemon-PATH lookup): staged tools are already
    // covered by the exec-root rule. Missing here is an internal error
    // (resolution already verified the file exists).
    let program_outside = !starts_with(exec_root, program);
    if program_outside {
        caps = caps
            .allow_file(program, nono::AccessMode::Read)
            .map_err(|e| format!("sandbox program {}: {e}", program.display()))?;
    }
    // A script's interpreter (the kernel opens that path at exec time),
    // collected whether the program itself is staged or not, plus
    // platform exec variants (macOS's /bin/sh is a stub that re-execs
    // /bin/bash: without the variant the spawn fails EPERM). Anything
    // ungrantable fails closed at exec with a debug log here.
    let mut extras = Vec::new();
    if let Some(interpreter) = script_interpreter(program)
        && !starts_with(exec_root, &interpreter)
    {
        extras.push(interpreter);
    }
    {
        let mut for_variants = extras.clone();
        if program_outside {
            for_variants.push(program.to_path_buf());
        }
        extras.extend(exec_variants(&for_variants));
    }
    for executable in &extras {
        if starts_with(exec_root, executable) {
            continue;
        }
        if nono::CapabilitySet::new()
            .allow_file(executable, nono::AccessMode::Read)
            .map(|with_file| {
                for cap in with_file.fs_capabilities() {
                    caps.add_fs(cap.clone());
                }
            })
            .is_err()
        {
            tracing::debug!(
                executable = %executable.display(),
                "sandbox: executable not grantable, exec fails closed if needed",
            );
            continue;
        }
        // Seatbelt mediates execution as a distinct
        // `process-exec-interpreter` operation that
        // `(allow process-exec*)` does not cover. Ignored on Linux,
        // where execute rights ride with read access.
        match exec_interpreter_rules(executable) {
            Ok(rules) => {
                for rule in rules {
                    caps = caps
                        .platform_rule(rule)
                        .map_err(|e| format!("sandbox exec rule: {e}"))?;
                }
            }
            Err(e) => {
                tracing::debug!(
                    executable = %executable.display(),
                    error = %e,
                    "sandbox: no exec rule, exec fails closed if needed",
                );
            }
        }
    }
    // The program itself needs the same Seatbelt exec rule when it lives
    // outside the staged tree (direct Mach-O exec is also mediated as
    // `process-exec-interpreter`).
    if program_outside {
        for rule in exec_interpreter_rules(program).map_err(|e| format!("sandbox exec rule: {e}"))? {
            caps = caps
                .platform_rule(rule)
                .map_err(|e| format!("sandbox exec rule: {e}"))?;
        }
    }
    // The loader and basic tools need their system paths; without these
    // nothing can start. Read-only, and skipped when absent so one policy
    // covers every distro. Everything else stays denied. (The macOS set
    // mirrors nono's own sandboxed-exec tests.)
    for path in system_read_paths() {
        match nono::CapabilitySet::new().allow_path(&path, nono::AccessMode::Read) {
            Ok(with_path) => {
                for cap in with_path.fs_capabilities() {
                    caps.add_fs(cap.clone());
                }
            }
            Err(e) => {
                tracing::debug!(path = %path.display(), error = %e, "sandbox: skipping system path");
            }
        }
    }
    for device in system_devices() {
        match nono::CapabilitySet::new().allow_file(&device, nono::AccessMode::Read) {
            Ok(with_device) => {
                for cap in with_device.fs_capabilities() {
                    caps.add_fs(cap.clone());
                }
            }
            Err(e) => {
                tracing::debug!(device = %device.display(), error = %e, "sandbox: skipping device");
            }
        }
    }
    Ok(caps.block_network())
}

/// True when `path` is `root` or lives under it, lexically.
#[cfg(unix)]
fn starts_with(root: &Path, path: &Path) -> bool {
    path.starts_with(root)
}

/// Extra binaries an executable re-execs as part of launching. macOS's
/// /bin/sh is a stub that re-execs /bin/bash ("as variant" in the
/// denial log): granting sh without bash fails the spawn with EPERM.
#[cfg(unix)]
fn exec_variants(executables: &[PathBuf]) -> Vec<PathBuf> {
    let mut variants = Vec::new();
    for path in executables {
        // Compare resolved paths (/bin/sh is a real file, not a
        // symlink, but resolve anyway for robustness).
        let resolved = path.canonicalize().unwrap_or_else(|_| path.clone());
        if resolved == Path::new("/bin/sh") {
            variants.push(PathBuf::from("/bin/bash"));
        }
    }
    variants
}

/// Seatbelt rules permitting execution of one granted binary: one
/// `(allow process-exec-interpreter ...)` line per path spelling
/// (resolved, plus the original when it differs, mirroring nono's
/// own file rules so symlinked paths match however the kernel
/// spells them).
#[cfg(unix)]
fn exec_interpreter_rules(path: &Path) -> Result<Vec<String>, String> {
    let resolved = path
        .canonicalize()
        .map_err(|e| format!("cannot canonicalize {}: {e}", path.display()))?;
    let mut paths = vec![resolved];
    if path != paths[0].as_path() {
        paths.push(path.to_path_buf());
    }
    paths
        .into_iter()
        .map(|path| {
            let text = path
                .to_str()
                .ok_or_else(|| format!("non-UTF-8 path {}", path.display()))?;
            let mut escaped = String::with_capacity(text.len());
            for c in text.chars() {
                match c {
                    '\\' => escaped.push_str("\\\\"),
                    '"' => escaped.push_str("\\\""),
                    c if c.is_control() => {
                        return Err(format!("control character in path {}", path.display()));
                    }
                    _ => escaped.push(c),
                }
            }
            Ok(format!(
                "(allow process-exec-interpreter (literal \"{escaped}\"))"
            ))
        })
        .collect()
}

/// Interpreter of a script (`#!` line), when absolute. Reads only the
/// first line; anything unreadable or relative yields `None` (the exec
/// then fails closed on its own).
#[cfg(unix)]
fn script_interpreter(program: &Path) -> Option<PathBuf> {
    use std::io::BufRead;
    let file = std::fs::File::open(program).ok()?;
    let mut lines = std::io::BufReader::new(file).lines();
    let first = lines.next()?.ok()?;
    let rest = first.strip_prefix("#!")?.trim_start();
    let interpreter = rest.split_whitespace().next()?;
    let path = PathBuf::from(interpreter);
    path.is_absolute().then_some(path)
}

/// Minimal read-only system paths needed to launch anything: the dynamic
/// loader and its libraries, nothing else. Toolchains are hermetic, so
/// system tool directories (`/usr/bin`, `/bin`, ...) stay denied — only
/// the specific program binary (plus a script interpreter) is granted
/// above. Best-effort: entries missing on this machine are skipped by
/// the caller.
#[cfg(unix)]
fn system_read_paths() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        // Dyld and the shared cache. Notably absent vs nono's own test
        // set: /usr, /bin, /Library, /private/var/db — denied here.
        vec![PathBuf::from("/System")]
    }
    #[cfg(target_os = "linux")]
    {
        // The ELF interpreter and libc live here (on merged-/usr
        // systems /lib is a symlink into /usr/lib, but the rule covers
        // only this subtree, not all of /usr).
        vec![PathBuf::from("/lib"), PathBuf::from("/lib64")]
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Vec::new()
    }
}

/// Device files an action may legitimately open (`>/dev/null` in shell
/// scripts, entropy reads). Narrower than a `/dev` directory rule.
#[cfg(unix)]
fn system_devices() -> Vec<PathBuf> {
    #[cfg(unix)]
    {
        vec![
            PathBuf::from("/dev/null"),
            PathBuf::from("/dev/zero"),
            PathBuf::from("/dev/urandom"),
        ]
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("reapid-test-{}-{}-sandbox", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn capabilities_cover_working_set_and_block_network() {
        let root = test_root("caps");
        let program = root.join("prog");
        std::fs::write(&program, b"#!/bin/sh\necho hi\n").unwrap();
        let caps = build_capabilities(&root, &program).unwrap();
        assert!(caps.is_network_blocked());
        // Exec root read/write (capabilities store canonicalized paths,
        // so /var resolves to /private/var here); the staged program
        // needs no extra rule.
        let resolved = root.canonicalize().unwrap();
        assert!(caps.fs_capabilities().iter().any(|c| c.resolved == resolved
            && c.access == nono::AccessMode::ReadWrite
            && !c.is_file));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn capabilities_cover_external_program() {
        let root = test_root("ext");
        let caps = build_capabilities(&root, Path::new("/bin/sh")).unwrap();
        assert!(caps.fs_capabilities().iter().any(|c| c.is_file
            && c.access == nono::AccessMode::Read));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_exec_root_fails() {
        let root = test_root("missing");
        let _ = std::fs::remove_dir_all(&root);
        assert!(build_capabilities(&root.join("nope"), Path::new("/bin/sh")).is_err());
    }

    #[test]
    fn script_interpreter_parses_absolute_shebangs() {
        let root = test_root("shebang");
        let script = root.join("run.sh");
        std::fs::write(&script, b"#!/bin/sh\necho hi\n").unwrap();
        assert_eq!(script_interpreter(&script), Some(PathBuf::from("/bin/sh")));
        let env_script = root.join("env.sh");
        std::fs::write(&env_script, b"#!/usr/bin/env python3\n").unwrap();
        assert_eq!(
            script_interpreter(&env_script),
            Some(PathBuf::from("/usr/bin/env"))
        );
        let relative = root.join("rel.sh");
        std::fs::write(&relative, b"#!tools/prog\n").unwrap();
        assert_eq!(script_interpreter(&relative), None);
        let binary = root.join("bin");
        std::fs::write(&binary, b"\x7fELF-not-really").unwrap();
        assert_eq!(script_interpreter(&binary), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
