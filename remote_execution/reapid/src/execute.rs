//! Host-side action execution for the Execution API.
//!
//! Unlike the cache APIs (which never run anything), `Execute` runs the
//! action's command as a subprocess on this host: inputs are staged from
//! the CAS into a fresh temp dir, the command runs with the daemon's
//! environment overlaid by the client-supplied variables, outputs are
//! collected back into the CAS, and the result is stored in the action
//! cache.
//!
//! This is what lets a network-isolated build work: toolchains and other
//! downloads are ordinary actions here. The populating build executes
//! them (network up) and the store keeps their outputs; the offline build
//! replays the same actions as action-cache hits, fetching every byte
//! from the local store.
//!
//! Design notes (v1 limitations, all deliberate):
//!
//! - Immediate completion: every `Execute` returns a single done
//!   `Operation`. There is no `WaitExecution`, no operation polling, and
//!   no execution queue. Clients recover from gaps by retrying `Execute`,
//!   and retries AC-hit, so nothing is lost or double-counted.
//! - No sandbox beyond process isolation plus a fresh input tree: the
//!   staged inputs are hermetic, but the command still sees the host's
//!   absolute paths, `/tmp`, and (during population) the network. Run
//!   only behind loopback / trusted callers.
//! - The action inherits the daemon's environment (`HOME`, `TMPDIR`,
//!   `PATH`, ...); the action's declared variables override it. Timeouts come from
//!   `Action.timeout`, if set; otherwise the action runs unbounded.
//! - Concurrency is bounded by available parallelism, so a many-action
//!   build cannot fork the host to death.
//! - Completed results are cached even when the exit code is nonzero
//!   (deterministic actions produce deterministic results); only
//!   infrastructure failures skip the cache. Clients MUST NOT rely on
//!   `inline_*` flags: small stdout/stderr is inlined, larger output goes
//!   to CAS by digest, and clients are required to handle both.
//! - `do_not_cache` actions still execute (and are still returned) but
//!   are never stored. `skip_cache_lookup` forces re-execution.
//! - `execution_policy` and `results_cache_policy` are accepted but not
//!   acted on: there is one worker (this host) and retention follows the
//!   store-wide eviction rules.
//!
//! REAPI v2.1/v2.2 deprecated several fields this server must still
//! accept: clients in the wild send `output_files`/`output_directories`
//! and `Command.platform` instead of (or alongside) `output_paths` and
//! `Action.platform`. The deprecated spellings are honored for
//! compatibility; where both are present the modern ones win.

#![allow(deprecated)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use prost::Message;

use crate::digest::{normalize_hash, DigestFunction};
use crate::services::{now_unix, Shared};
use crate::store::{EntryKey, EntryKind, Hints, Store};

/// Stdout/stderr at or under this size is inlined in the `ActionResult`;
/// larger output is stored in CAS and referenced by digest.
const INLINE_OUTPUT_LIMIT: usize = 1024 * 1024;

/// Sequence for unique exec dirs, appended after the daemon pid.
pub static EXEC_SEQ: AtomicU64 = AtomicU64::new(0);

/// Check an action's platform requirements against this host. `Command`
/// requirements apply first, `Action` requirements overlay them (the REAPI
/// deprecation order: servers prefer the action's). Unknown requirement
/// keys are ignored so future clients keep working; known keys must match
/// exactly.
fn check_platform(
    action: &reapi::Action,
    command: &reapi::Command,
    worker: &BTreeMap<String, String>,
) -> Result<(), tonic::Status> {
    let mut required = BTreeMap::new();
    for platform in command.platform.iter() {
        for p in &platform.properties {
            required.insert(p.name.clone(), p.value.clone());
        }
    }
    if let Some(platform) = &action.platform {
        for p in &platform.properties {
            required.insert(p.name.clone(), p.value.clone());
        }
    }
    for (key, value) in &required {
        if let Some(have) = worker.get(key) {
            if have != value {
                return Err(tonic::Status::failed_precondition(format!(
                    "action requires {key}={value} but this host provides {key}={have}"
                )));
            }
        } else {
            tracing::debug!(platform_key = %key, "ignoring unknown platform requirement");
        }
    }
    Ok(())
}

/// Join a REAPI relative path (`/`-separated, no leading slash or `..`)
/// onto a root. Returns `None` for paths that would escape the root.
fn join_rel(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') {
        return None;
    }
    let mut path = root.to_path_buf();
    for part in rel.split('/') {
        match part {
            "" | "." => return None,
            ".." => return None,
            name => path.push(name),
        }
    }
    Some(path)
}

/// Fetch one blob by digest, verifying its size. Returns `Ok(None)` for
/// a missing blob (recorded in `missing` so one response can name them
/// all); malformed references fail immediately.
async fn fetch_blob(
    store: &Store,
    digest: &reapi::Digest,
    proto_function: i32,
    missing: &mut Vec<String>,
) -> Result<Option<Vec<u8>>, tonic::Status> {
    if digest.size_bytes < 0 {
        return Err(tonic::Status::invalid_argument("negative blob size"));
    }
    if digest.size_bytes == 0 {
        return Ok(Some(Vec::new()));
    }
    let Some(hash) = normalize_hash(&digest.hash) else {
        return Err(tonic::Status::invalid_argument("blob hash is not hex"));
    };
    let Some((function, meta)) = store.cas_lookup(&hash, proto_function) else {
        missing.push(format!("blobs/{hash}/{}", digest.size_bytes));
        return Ok(None);
    };
    if meta.size_bytes != digest.size_bytes as u64 {
        missing.push(format!("blobs/{hash}/{}", digest.size_bytes));
        return Ok(None);
    }
    let path = store.cas_data_path(function, &hash);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|e| tonic::Status::internal(format!("store read failed: {e}")))?;
    Ok(Some(bytes))
}

/// Stage one input directory (recursively) under `dest`. File names must
/// be plain names (no separators); anything else is a malformed action.
async fn stage_dir(
    store: &Store,
    proto_function: i32,
    dir_digest: &reapi::Digest,
    dest: &Path,
    missing: &mut Vec<String>,
) -> Result<(), tonic::Status> {
    let bytes = match fetch_blob(store, dir_digest, proto_function, missing).await? {
        Some(bytes) => bytes,
        None => return Ok(()),
    };
    let dir = reapi::Directory::decode(&bytes[..])
        .map_err(|_| tonic::Status::invalid_argument("input directory does not decode"))?;
    for file in &dir.files {
        let Some(name) = plain_name(&file.name) else {
            return Err(tonic::Status::invalid_argument(format!(
                "bad input file name {:?}",
                file.name
            )));
        };
        let digest = file.digest.as_ref().ok_or_else(|| {
            tonic::Status::invalid_argument(format!("input file {name:?} has no digest"))
        })?;
        let Some(bytes) = fetch_blob(store, digest, proto_function, missing).await? else {
            continue;
        };
        let path = dest.join(name);
        tokio::fs::write(&path, &bytes)
            .await
            .map_err(|e| tonic::Status::internal(format!("staging write failed: {e}")))?;
        #[cfg(unix)]
        if file.is_executable {
            use std::os::unix::fs::PermissionsExt;
            let perm = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&path, perm)
                .map_err(|e| tonic::Status::internal(format!("staging chmod failed: {e}")))?;
        }
        touch_input(store, digest, proto_function);
    }
    for sub in &dir.directories {
        let Some(name) = plain_name(&sub.name) else {
            return Err(tonic::Status::invalid_argument(format!(
                "bad input directory name {:?}",
                sub.name
            )));
        };
        let digest = sub.digest.as_ref().ok_or_else(|| {
            tonic::Status::invalid_argument(format!("input directory {name:?} has no digest"))
        })?;
        let path = dest.join(name);
        tokio::fs::create_dir_all(&path)
            .await
            .map_err(|e| tonic::Status::internal(format!("staging mkdir failed: {e}")))?;
        Box::pin(stage_dir(store, proto_function, digest, &path, missing)).await?;
    }
    for link in &dir.symlinks {
        let Some(name) = plain_name(&link.name) else {
            return Err(tonic::Status::invalid_argument(format!(
                "bad input symlink name {:?}",
                link.name
            )));
        };
        #[cfg(unix)]
        std::os::unix::fs::symlink(&link.target, dest.join(name))
            .map_err(|e| tonic::Status::internal(format!("staging symlink failed: {e}")))?;
        #[cfg(not(unix))]
        return Err(tonic::Status::failed_precondition(
            "input symlinks are unsupported on this host",
        ));
    }
    Ok(())
}

/// A bare file or directory name: no separators, no `.`/`..`, non-empty.
fn plain_name(name: &str) -> Option<&str> {
    match name {
        "" | "." | ".." => None,
        _ if name.contains('/') || name.contains('\\') => None,
        _ => Some(name),
    }
}

/// Resolve the program to run: absolute paths as-is, relative paths with
/// separators strictly under the exec root, bare names first under the
/// exec root then on the daemon's `PATH` (standard shell semantics for
/// bare commands like `sh`).
fn resolve_program(exec_root: &Path, argv0: &str) -> Result<PathBuf, tonic::Status> {
    if argv0.is_empty() {
        return Err(tonic::Status::invalid_argument("empty command"));
    }
    let rel = Path::new(argv0);
    if rel.is_absolute() {
        if rel.is_file() {
            return Ok(rel.to_path_buf());
        }
        return Err(tonic::Status::failed_precondition(format!(
            "executable {argv0:?} not found"
        )));
    }
    if argv0.contains('/') {
        let Some(path) = join_rel(exec_root, argv0) else {
            return Err(tonic::Status::invalid_argument(format!(
                "bad executable path {argv0:?}"
            )));
        };
        if path.is_file() {
            return Ok(path);
        }
        return Err(tonic::Status::failed_precondition(format!(
            "executable {argv0:?} not staged"
        )));
    }
    let staged = exec_root.join(argv0);
    if staged.is_file() {
        return Ok(staged);
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(argv0);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(tonic::Status::failed_precondition(format!(
        "executable {argv0:?} not found"
    )))
}

/// Best-effort LRU touch for a staged input, so a big build's working set
/// is not evicted out from under it. Touching a function variant that is
/// not present is a no-op, so every candidate is touched.
fn touch_input(store: &Store, digest: &reapi::Digest, proto_function: i32) {
    let Some(hash) = normalize_hash(&digest.hash) else {
        return;
    };
    let functions: Vec<DigestFunction> = if proto_function != 0 {
        DigestFunction::from_proto(proto_function).into_iter().collect()
    } else {
        DigestFunction::by_hex_len(hash.len()).to_vec()
    };
    let now = now_unix();
    for function in functions {
        store.touch(
            &EntryKey {
                kind: EntryKind::Cas,
                function,
                hash: hash.clone(),
            },
            now,
        );
    }
}

/// Store `bytes` in CAS under `function`, returning its digest.
async fn store_blob(
    store: &Store,
    function: DigestFunction,
    bytes: &[u8],
    hints: &Hints,
    now: u64,
) -> Result<reapi::Digest, tonic::Status> {
    let hash = function.hexdigest(bytes);
    let size = bytes.len() as u64;
    store
        .cas_put(bytes, &hash, size, function.to_proto(), hints, now)
        .await
        .map_err(|e| tonic::Status::internal(format!("output store failed: {e}")))?;
    Ok(reapi::Digest {
        hash,
        size_bytes: size as i64,
    })
}

/// Collect one output directory tree, storing every file and subdirectory.
/// Entries are visited in byte-sorted name order and children post-order,
/// so the tree encoding (and its digest) is deterministic. Returns the
/// root directory proto and its digest; nested directories accumulate in
/// `children`, deduplicated by digest.
#[allow(clippy::too_many_arguments)]
async fn build_tree(
    store: &Store,
    function: DigestFunction,
    dir_path: &Path,
    hints: &Hints,
    now: u64,
    children: &mut Vec<reapi::Directory>,
    seen: &mut BTreeSet<String>,
) -> Result<(reapi::Directory, reapi::Digest), tonic::Status> {
    let mut names: Vec<std::ffi::OsString> = Vec::new();
    let mut entries = tokio::fs::read_dir(dir_path)
        .await
        .map_err(|e| tonic::Status::internal(format!("output walk failed: {e}")))?;
    while let Some(entry) = entries
        .next_entry()
        .await
        .map_err(|e| tonic::Status::internal(format!("output walk failed: {e}")))?
    {
        names.push(entry.file_name());
    }
    names.sort();
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let mut symlinks = Vec::new();
    for name in names {
        let Some(name_str) = name.to_str() else {
            return Err(tonic::Status::internal("non-UTF-8 output name"));
        };
        let path = dir_path.join(&name);
        let meta = tokio::fs::symlink_metadata(&path)
            .await
            .map_err(|e| tonic::Status::internal(format!("output stat failed: {e}")))?;
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            let target = tokio::fs::read_link(&path)
                .await
                .map_err(|e| tonic::Status::internal(format!("output readlink failed: {e}")))?;
            symlinks.push(reapi::SymlinkNode {
                name: name_str.to_string(),
                target: target.to_string_lossy().into_owned(),
                node_properties: None,
            });
        } else if file_type.is_dir() {
            let (child_proto, child_digest) = Box::pin(build_tree(
                store, function, &path, hints, now, children, seen,
            ))
            .await?;
            if seen.insert(child_digest.hash.clone()) {
                children.push(child_proto);
            }
            directories.push(reapi::DirectoryNode {
                name: name_str.to_string(),
                digest: Some(child_digest),
            });
        } else if file_type.is_file() {
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|e| tonic::Status::internal(format!("output read failed: {e}")))?;
            let digest = store_blob(store, function, &bytes, hints, now).await?;
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            files.push(reapi::FileNode {
                name: name_str.to_string(),
                digest: Some(digest),
                is_executable: executable,
                node_properties: None,
            });
        } else {
            // Sockets, fifos, devices: no REAPI representation exists, and
            // silently dropping outputs hides breakage less than failing.
            // Builds do not produce these as declared outputs in practice.
            tracing::debug!(path = %path.display(), "skipping special output file");
        }
    }
    let dir = reapi::Directory {
        files,
        directories,
        symlinks,
        node_properties: None,
    };
    let mut bytes = Vec::new();
    dir.encode(&mut bytes)
        .map_err(|_| tonic::Status::internal("directory encode failed"))?;
    let digest = reapi::Digest {
        hash: function.hexdigest(&bytes),
        size_bytes: bytes.len() as i64,
    };
    store_blob(store, function, &bytes, hints, now).await?;
    Ok((dir, digest))
}

/// An execution failure after setup: reported in the response status (and
/// never cached), not as a gRPC error.
fn failed(code: tonic::Code, message: String) -> reapi::ExecuteResponse {
    reapi::ExecuteResponse {
        result: None,
        cached_result: false,
        status: Some(crate::services::response_status(code, message)),
        server_logs: Default::default(),
        message: String::new(),
    }
}

/// Action timeout as a duration. Invalid values run unbounded (with a
/// debug log) rather than failing the action.
fn action_timeout(action: &reapi::Action) -> Option<std::time::Duration> {
    let t = action.timeout.as_ref()?;
    if t.seconds < 0 || t.nanos < 0 || t.nanos >= 1_000_000_000 {
        tracing::debug!("ignoring invalid action timeout");
        return None;
    }
    Some(std::time::Duration::new(t.seconds as u64, t.nanos as u32))
}

/// Run one action end-to-end (the caller holds an execution permit):
/// stage inputs, spawn, collect outputs, store the result unless told not
/// to. Returns a completed response; infrastructure failures that escape
/// collection are gRPC errors.
#[allow(clippy::too_many_arguments)]
async fn run_action(
    store: &Store,
    action: &reapi::Action,
    command: &reapi::Command,
    out_function: DigestFunction,
    hints: &Hints,
    now: u64,
    cwd: &Path,
    program: &Path,
) -> Result<reapi::ExecuteResponse, tonic::Status> {
    // Pre-create parent dirs of declared outputs: the spec requires the
    // worker to make them even when they are not part of the input root.
    let mut wanted: Vec<String> = Vec::new();
    for list in [
        &command.output_files,
        &command.output_directories,
        &command.output_paths,
    ] {
        for path in list {
            if !wanted.contains(path) {
                wanted.push(path.clone());
            }
        }
    }
    for rel in &wanted {
        let Some(path) = join_rel(cwd, rel) else {
            continue;
        };
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                tonic::Status::internal(format!("staging output parents failed: {e}"))
            })?;
        }
    }

    // std::process (not tokio::process: its `process` feature would pull
    // an unvendored `signal-hook-registry` into third-party).
    let mut cmd = std::process::Command::new(program);
    cmd.args(&command.arguments[1.min(command.arguments.len())..]);
    // Host execution starts from the daemon's own environment (HOME,
    // TMPDIR, PATH, ...) so tools behave as they do under a local
    // `buck2 build`; the action's declared variables override it.
    cmd.envs(std::env::vars());
    for var in &command.environment_variables {
        cmd.env(&var.name, &var.value);
    }
    cmd.current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| tonic::Status::internal(format!("spawn failed: {e}")))?;
    // Both pipes drain on blocking-pool threads from the start: a child
    // blocked on a full pipe would otherwise never exit for the wait loop.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_reader = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Some(ref mut pipe) = out_pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let err_reader = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut buf = Vec::new();
        if let Some(ref mut pipe) = err_pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let deadline = action_timeout(action).map(|d| tokio::time::Instant::now() + d);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => return Err(tonic::Status::internal(format!("wait failed: {e}"))),
        }
        if deadline.is_some_and(|d| tokio::time::Instant::now() >= d) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(failed(
                tonic::Code::DeadlineExceeded,
                "action timed out".to_string(),
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    let stdout = out_reader
        .await
        .map_err(|_| tonic::Status::internal("stdout reader failed"))?;
    let stderr = err_reader
        .await
        .map_err(|_| tonic::Status::internal("stderr reader failed"))?;
    let exit_code = match status.code() {
        Some(code) => code,
        None => {
            return Ok(failed(
                tonic::Code::Internal,
                "action terminated by signal".to_string(),
            ));
        }
    };

    // Collect outputs. With output_paths set, the deprecated lists are
    // ignored per the spec; missing paths are silently absent only there.
    let (explicit, auto) = if command.output_paths.is_empty() {
        (
            command
                .output_files
                .iter()
                .chain(command.output_directories.iter())
                .cloned()
                .collect::<Vec<_>>(),
            false,
        )
    } else {
        (command.output_paths.clone(), true)
    };
    let mut seen_paths = BTreeSet::new();
    let mut output_files = Vec::new();
    let mut output_symlinks = Vec::new();
    let mut output_directories = Vec::new();
    for rel in explicit {
        if !seen_paths.insert(rel.clone()) {
            continue;
        }
        let Some(path) = join_rel(cwd, &rel) else {
            return Ok(failed(
                tonic::Code::InvalidArgument,
                format!("bad output path {rel:?}"),
            ));
        };
        let meta = match tokio::fs::symlink_metadata(&path).await {
            Ok(meta) => meta,
            Err(_) if auto => continue,
            Err(_) => {
                return Ok(failed(
                    tonic::Code::FailedPrecondition,
                    format!("declared output {rel:?} missing"),
                ))
            }
        };
        let file_type = meta.file_type();
        if file_type.is_symlink() {
            let target = tokio::fs::read_link(&path).await.map_err(|e| {
                tonic::Status::internal(format!("output readlink failed: {e}"))
            })?;
            output_symlinks.push(reapi::OutputSymlink {
                path: rel,
                target: target.to_string_lossy().into_owned(),
                node_properties: None,
            });
        } else if file_type.is_dir() {
            let mut children = Vec::new();
            let mut seen = BTreeSet::new();
            let (root_proto, root_digest) = Box::pin(build_tree(
                store, out_function, &path, hints, now, &mut children, &mut seen,
            ))
            .await?;
            let mut tree_bytes = Vec::new();
            reapi::Tree {
                root: Some(root_proto),
                children,
            }
            .encode(&mut tree_bytes)
            .map_err(|_| tonic::Status::internal("tree encode failed"))?;
            let tree_digest = store_blob(store, out_function, &tree_bytes, hints, now).await?;
            output_directories.push(reapi::OutputDirectory {
                path: rel,
                tree_digest: Some(tree_digest),
                is_topologically_sorted: true,
                root_directory_digest: Some(root_digest),
            });
        } else if file_type.is_file() {
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|e| tonic::Status::internal(format!("output read failed: {e}")))?;
            let digest = store_blob(store, out_function, &bytes, hints, now).await?;
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            output_files.push(reapi::OutputFile {
                path: rel,
                digest: Some(digest),
                is_executable: executable,
                contents: Vec::new(),
                node_properties: None,
            });
        } else if !auto {
            return Ok(failed(
                tonic::Code::FailedPrecondition,
                format!("declared output {rel:?} is not a file or directory"),
            ));
        }
    }

    let (stdout_raw, stdout_digest) = inline_or_store(store, out_function, stdout, hints, now).await?;
    let (stderr_raw, stderr_digest) = inline_or_store(store, out_function, stderr, hints, now).await?;
    Ok(reapi::ExecuteResponse {
        result: Some(reapi::ActionResult {
            output_files,
            output_file_symlinks: Vec::new(),
            output_symlinks,
            output_directories,
            output_directory_symlinks: Vec::new(),
            exit_code,
            stdout_raw,
            stdout_digest,
            stderr_raw,
            stderr_digest,
            execution_metadata: Some(reapi::ExecutedActionMetadata {
                worker: crate::config::ExecutionConfig::worker_name(),
                queued_timestamp: None,
                worker_start_timestamp: None,
                worker_completed_timestamp: None,
                input_fetch_start_timestamp: None,
                input_fetch_completed_timestamp: None,
                execution_start_timestamp: None,
                execution_completed_timestamp: None,
                virtual_execution_duration: None,
                output_upload_start_timestamp: None,
                output_upload_completed_timestamp: None,
                auxiliary_metadata: Vec::new(),
            }),
        }),
        cached_result: false,
        status: Some(crate::services::response_status(tonic::Code::Ok, String::new())),
        server_logs: Default::default(),
        message: String::new(),
    })
}

/// Inline small stdout/stderr; store larger output in CAS and reference it.
async fn inline_or_store(
    store: &Store,
    function: DigestFunction,
    bytes: Vec<u8>,
    hints: &Hints,
    now: u64,
) -> Result<(Vec<u8>, Option<reapi::Digest>), tonic::Status> {
    if bytes.len() <= INLINE_OUTPUT_LIMIT {
        return Ok((bytes, None));
    }
    let digest = store_blob(store, function, &bytes, hints, now).await?;
    Ok((Vec::new(), Some(digest)))
}

/// Fresh exec root under the system temp dir. Removed (best-effort) when
/// the execution finishes, whatever its outcome.
fn fresh_exec_root() -> Result<PathBuf, tonic::Status> {
    let seq = EXEC_SEQ.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("reapid-exec-{}-{seq}", std::process::id()));
    std::fs::create_dir_all(&root)
        .map_err(|e| tonic::Status::internal(format!("exec root failed: {e}")))?;
    Ok(root)
}

/// Execute one action request against the local store: action-cache hit
/// returns immediately, otherwise the action runs on this host and the
/// result is stored (unless `do_not_cache`). The caller holds an execution
/// permit for the whole call.
pub async fn run(
    shared: &Shared,
    hints: &Hints,
    req: reapi::ExecuteRequest,
) -> Result<reapi::ExecuteResponse, tonic::Status> {
    let action_digest = req.action_digest.as_ref().ok_or_else(|| {
        tonic::Status::invalid_argument("execute without an action digest")
    })?;
    if action_digest.size_bytes < 0 {
        return Err(tonic::Status::invalid_argument("negative action size"));
    }
    let Some(action_hash) = normalize_hash(&action_digest.hash) else {
        return Err(tonic::Status::invalid_argument("action hash is not hex"));
    };
    let store = shared.local_store();
    let now = now_unix();
    let proto_function = req.digest_function;

    // Cache lookup first (unless skipped): identical bytes to
    // `get_action_result`, so plain cache reads and executions agree.
    if !req.skip_cache_lookup
        && let Some((function, _)) = store.ac_lookup(&action_hash, proto_function)
    {
        let path = store.ac_data_path(function, &action_hash);
        let cached = match tokio::fs::read(&path).await {
            Ok(bytes) => match reapi::ActionResult::decode(&bytes[..]) {
                Ok(result) => Some(result),
                Err(e) => {
                    tracing::warn!(hash = %action_hash, error = %e, "dropping corrupt cached action result");
                    store
                        .remove(&EntryKey {
                            kind: EntryKind::ActionCache,
                            function,
                            hash: action_hash.clone(),
                        })
                        .await;
                    None
                }
            },
            Err(_) => None,
        };
        if let Some(result) = cached {
            let _ = store
                .record_access(
                    &EntryKey {
                        kind: EntryKind::ActionCache,
                        function,
                        hash: action_hash.clone(),
                    },
                    now,
                )
                .await;
            tracing::debug!("execute: action-cache hit");
            return Ok(reapi::ExecuteResponse {
                result: Some(result),
                cached_result: true,
                status: Some(crate::services::response_status(
                    tonic::Code::Ok,
                    String::new(),
                )),
                server_logs: Default::default(),
                message: String::new(),
            });
        }
    }

    // Fetch the action and command blobs.
    let mut missing = Vec::new();
    let action_digest_proto = reapi::Digest {
        hash: action_hash.clone(),
        size_bytes: action_digest.size_bytes,
    };
    let action_bytes = match fetch_blob(store, &action_digest_proto, proto_function, &mut missing).await? {
        Some(bytes) => bytes,
        None => {
            return Err(tonic::Status::failed_precondition(format!(
                "missing inputs: {}",
                missing.join(", ")
            )))
        }
    };
    let action = reapi::Action::decode(&action_bytes[..])
        .map_err(|_| tonic::Status::invalid_argument("action does not decode"))?;
    let command_digest = action.command_digest.as_ref().ok_or_else(|| {
        tonic::Status::invalid_argument("action without a command digest")
    })?;
    let command_bytes = match fetch_blob(store, command_digest, proto_function, &mut missing).await? {
        Some(bytes) => bytes,
        None => {
            return Err(tonic::Status::failed_precondition(format!(
                "missing inputs: {}",
                missing.join(", ")
            )))
        }
    };
    let command = reapi::Command::decode(&command_bytes[..])
        .map_err(|_| tonic::Status::invalid_argument("command does not decode"))?;

    // Platform match before touching the filesystem: unknown requirements
    // (future clients) pass through, known ones must match this host.
    let worker = shared.config.execution.worker_properties();
    tracing::debug!(
        platform = ?command.platform,
        action_platform = ?action.platform,
        worker = ?worker,
        "execute: platform check"
    );
    check_platform(&action, &command, &worker)?;

    // Digest function for output blobs: the request's, else sha256. (The AC
    // entry itself is keyed symmetrically with lookups, which pass the raw
    // request value through.)
    let out_function = DigestFunction::from_proto(proto_function).unwrap_or(DigestFunction::Sha256);

    let exec_root = fresh_exec_root()?;
    let outcome = execute_inner(
        shared,
        &action_hash,
        &action,
        &command,
        out_function,
        hints,
        now,
        proto_function,
        &exec_root,
    )
    .await;
    let _ = tokio::fs::remove_dir_all(&exec_root).await;
    outcome
}

/// The fallible middle of [`run`]: stage inputs, execute, collect and
/// store outputs. Runs under the execution semaphore.
#[allow(clippy::too_many_arguments)]
async fn execute_inner(
    shared: &Shared,
    action_hash: &str,
    action: &reapi::Action,
    command: &reapi::Command,
    out_function: DigestFunction,
    hints: &Hints,
    now: u64,
    proto_function: i32,
    exec_root: &Path,
) -> Result<reapi::ExecuteResponse, tonic::Status> {
    let store = shared.local_store();
    let argv0 = command.arguments.first().ok_or_else(|| {
        tonic::Status::invalid_argument("command has no argv")
    })?;

    // Working directory, relative to the input root (empty means root).
    let cwd = if command.working_directory.is_empty() {
        exec_root.to_path_buf()
    } else {
        join_rel(exec_root, &command.working_directory).ok_or_else(|| {
            tonic::Status::invalid_argument(format!(
                "bad working directory {:?}",
                command.working_directory
            ))
        })?
    };

    // Stage the input tree (empty when the action declares no inputs).
    let mut missing = Vec::new();
    if let Some(root) = &action.input_root_digest
        && root.size_bytes != 0
    {
        tokio::fs::create_dir_all(exec_root)
            .await
            .map_err(|e| tonic::Status::internal(format!("staging root failed: {e}")))?;
        Box::pin(stage_dir(store, proto_function, root, exec_root, &mut missing)).await?;
    }
    if !missing.is_empty() {
        return Err(tonic::Status::failed_precondition(format!(
            "missing inputs: {}",
            missing.join(", ")
        )));
    }
    if !cwd.exists() {
        return Err(tonic::Status::failed_precondition(format!(
            "working directory {:?} not in inputs",
            command.working_directory
        )));
    }
    // Resolve the executable after staging: `argv[0]` is usually a tool
    // inside the staged inputs (resolving before staging looks at an
    // empty directory and always fails).
    let program = resolve_program(exec_root, argv0)?;

    let response =
        run_action(store, action, command, out_function, hints, now, cwd.as_path(), program.as_path())
            .await?;

    // Cache clean results unless told not to. A nonzero exit is never
    // stored, so a failed action always re-executes instead of replaying
    // a recorded failure. Non-OK statuses are never cached either
    // (REAPI: a result with an error status MUST NOT be).
    let cacheable = response
        .status
        .as_ref()
        .map(|s| s.code == tonic::Code::Ok as i32)
        .unwrap_or(false)
        && response
            .result
            .as_ref()
            .map(|r| r.exit_code == 0)
            .unwrap_or(false);
    if cacheable && !action.do_not_cache {
        if let Some(result) = &response.result {
            let mut bytes = Vec::new();
            result
                .encode(&mut bytes)
                .map_err(|_| tonic::Status::internal("result encode failed"))?;
            // ac_put takes the raw request digest-function value, symmetric
            // with the ac_lookup on the cache-hit path above.
            store
                .ac_put(action_hash, proto_function, &bytes, hints, now)
                .await
                .map_err(|e| {
                    tonic::Status::internal(format!("action cache store failed: {e}"))
                })?;
        }
        // Evict now: like every other mutating RPC, the store converges
        // before this call returns.
        shared.evict_pass().await;
    }
    Ok(response)
}
