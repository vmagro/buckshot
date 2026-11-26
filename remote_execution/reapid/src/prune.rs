//! Offline pruning of entries the most recent daemon process never touched.
//!
//! Every store access refreshes an on-disk timestamp: writes create files,
//! `record_access` rewrites the sidecar, and `Store::touch` bumps the data
//! file's mtime. `Store::open` records the process start (birth) in
//! `.reapid-birth`. Pruning deletes every entry whose files all predate a
//! `--since` instant (default: the birth marker), so only the most recent
//! process's working set survives for the cache upload. This is what keeps a
//! cache-carried store from growing without bound: entries no build touches
//! anymore fall out instead of accumulating forever.
//!
//! Deletion is mark-and-sweep, not pure recency: CAS blobs reachable from a
//! surviving action result are kept even when stale. An action-cache hit
//! whose output blobs are gone is a hard build error (the client does not
//! re-execute), so sweeping a stale blob out from under a live result would
//! trade disk for broken builds. Blobs referenced only by swept results are
//! unreferenced and go.
//!
//! Pruning is still always safe: `FindMissingBlobs` reports pruned digests
//! as missing and clients re-upload them.
//!
//! The pruner runs offline: stop the daemon first. It never opens the store
//! (opening would rewrite the birth marker); it only walks files and mtimes.
//! Pruning a live store would delete files the daemon's index still
//! references, so stop the daemon and prune before uploading the cache.

use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

use prost::Message;

use crate::digest::DigestFunction;
use crate::digest::normalize_hash;

/// Outcome of one prune pass.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PruneStats {
    /// Entry data files seen.
    pub scanned: usize,
    /// Entries kept (touched, or referenced by a kept action result).
    pub kept: usize,
    /// Entries deleted.
    pub removed: usize,
    /// Bytes deleted (entries, orphans, debris, partial uploads).
    pub freed_bytes: u64,
    /// Partial-upload files cleared.
    pub uploads_cleared: usize,
}

/// Delete every entry under `root` that predates `since` and is not
/// referenced by a surviving action result. Also sweeps what the next open
/// would drop anyway: `*.tmp-*` crash debris, orphan sidecars/compressed
/// forms, and stale partial uploads. `dry_run` reports without deleting.
pub async fn prune_untouched(
    root: &Path,
    since: SystemTime,
    dry_run: bool,
) -> std::io::Result<PruneStats> {
    let mut stats = PruneStats::default();
    // Phase 1: walk the tree, deciding each entry group's fate and deleting
    // debris/orphans (which no closure can reference back). Group deletions
    // are deferred until the closure is known.
    let mut plan = PrunePlan::default();
    for kind in ["cas", "ac"] {
        collect_kind(root, kind, since, dry_run, &mut stats, &mut plan).await?;
    }
    // Phase 2: pin every CAS blob a kept action result references, so the
    // sweep never orphans a surviving result. Overflow keeps all of CAS
    // (a weaker prune, but never a broken result).
    let pinned = collect_referenced_cas(root, &plan.kept_ac).await?;
    // Phase 3: apply the deferred group deletions.
    for group in &plan.groups {
        if group.touch_fresh {
            stats.kept += 1;
            continue;
        }
        if let Some(ref pinned) = pinned
            && pinned.contains(&group.hash)
        {
            stats.kept += 1;
            continue;
        }
        for path in &group.files {
            remove_file(path, dry_run, &mut stats).await?;
        }
        stats.removed += 1;
    }
    // Stale partial uploads never resume across restarts; the next open
    // would wipe them, so they must not ride along in a cache upload.
    let uploads = root.join("uploads");
    if let Ok(mut entries) = tokio::fs::read_dir(&uploads).await {
        while let Some(entry) = entries.next_entry().await? {
            if !entry.file_type().await?.is_file() {
                continue;
            }
            remove_file(&entry.path(), dry_run, &mut stats).await?;
            stats.uploads_cleared += 1;
        }
    }
    Ok(stats)
}

/// Deferred group deletions plus the kept action results that seed the
/// reference closure.
#[derive(Default)]
struct PrunePlan {
    groups: Vec<GroupDecision>,
    kept_ac: Vec<(DigestFunction, String)>,
}

/// One entry's files and its touch verdict. Deletion waits for the closure.
struct GroupDecision {
    /// Data file plus sidecar (plus the compressed form for CAS).
    files: Vec<PathBuf>,
    /// Any group file is at least as fresh as `--since`.
    touch_fresh: bool,
    /// Normalized content hash, for closure pinning.
    hash: String,
}

async fn collect_kind(
    root: &Path,
    kind: &str,
    since: SystemTime,
    dry_run: bool,
    stats: &mut PruneStats,
    plan: &mut PrunePlan,
) -> std::io::Result<()> {
    let kind_dir = root.join(kind);
    let mut fn_dirs = match tokio::fs::read_dir(&kind_dir).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    while let Some(fn_entry) = fn_dirs.next_entry().await? {
        // Only prune digest-function dirs the store owns; anything else is
        // foreign and left alone.
        let Some(function) =
            DigestFunction::from_dir_name(&fn_entry.file_name().to_string_lossy())
        else {
            continue;
        };
        if !fn_entry.file_type().await?.is_dir() {
            continue;
        }
        let mut shards = tokio::fs::read_dir(fn_entry.path()).await?;
        while let Some(shard_entry) = shards.next_entry().await? {
            if !shard_entry.file_type().await?.is_dir() {
                continue;
            }
            collect_shard(&shard_entry.path(), kind, function, since, dry_run, stats, plan).await?;
        }
    }
    Ok(())
}

/// Classify one shard directory: entry groups are recorded for deferred
/// deletion, while orphans and crash debris go immediately (nothing the
/// closure pins can bring them back: the closure only pins data files that
/// exist). The snapshot-then-act order keeps group deletes from
/// double-counting when a companion's turn comes.
#[allow(clippy::too_many_arguments)]
async fn collect_shard(
    shard: &Path,
    kind: &str,
    function: DigestFunction,
    since: SystemTime,
    dry_run: bool,
    stats: &mut PruneStats,
    plan: &mut PrunePlan,
) -> std::io::Result<()> {
    let mut names = Vec::new();
    let mut files = tokio::fs::read_dir(shard).await?;
    while let Some(entry) = files.next_entry().await? {
        if !entry.file_type().await?.is_file() {
            continue;
        }
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    // Groups first: their verdicts gate the orphan sweep below.
    // Kept groups' companions survive it; deleted groups' companions go
    // with the group delete (or would, under `--dry-run`).
    let mut live: HashSet<String> = HashSet::new();
    let mut dead: HashSet<String> = HashSet::new();
    for name in &names {
        if !is_data_file(name) {
            continue;
        }
        stats.scanned += 1;
        let mut group = vec![shard.join(name), meta_path(shard, name)];
        // Only CAS blobs have a compressed form; an `ac/*.zst` is always
        // garbage (mirrors the open-time scan), and rides with the group.
        group.push(zst_path(shard, name));
        // The `ac/*.zst` garbage does not vote on freshness.
        let fresh_paths: &[PathBuf] = match kind {
            "ac" => &group[..2],
            _ => &group,
        };
        let touch_fresh = any_fresh(fresh_paths, since).await;
        if touch_fresh {
            live.insert(name.clone());
            if kind == "ac" {
                plan.kept_ac.push((function, name.clone()));
            }
        } else {
            dead.insert(name.clone());
        }
        plan.groups.push(GroupDecision {
            files: group,
            touch_fresh,
            hash: name.clone(),
        });
    }
    for name in &names {
        // Crash debris: temp files a write a crash interrupted mid-rename.
        if name.contains(".tmp-") {
            remove_file(&shard.join(name), dry_run, stats).await?;
            continue;
        }
        if let Some(base) = name.strip_suffix(".meta.json") {
            // Companions of kept or deleted groups stay out of the sweep:
            // the group delete takes them (or would, dry).
            if !live.contains(base) && !dead.contains(base) {
                remove_file(&shard.join(name), dry_run, stats).await?;
            }
            continue;
        }
        if let Some(base) = name.strip_suffix(".zst") {
            // CAS keeps the compressed form of live entries; action-cache
            // compressed forms are garbage either way.
            if kind == "ac" || (!live.contains(base) && !dead.contains(base)) {
                remove_file(&shard.join(name), dry_run, stats).await?;
            }
        }
        // Unknown files are left alone: prune only deletes what it owns.
    }
    Ok(())
}

/// True for an entry data file: a pure-hex content hash with no suffix.
/// Companion (`.meta.json`, `.zst`) and debris (`.tmp-*`) names never match.
fn is_data_file(name: &str) -> bool {
    !name.contains('.') && normalize_hash(name).is_some()
}

fn meta_path(shard: &Path, base: &str) -> PathBuf {
    let mut path = shard.join(base).into_os_string();
    path.push(".meta.json");
    PathBuf::from(path)
}

fn zst_path(shard: &Path, base: &str) -> PathBuf {
    let mut path = shard.join(base).into_os_string();
    path.push(".zst");
    PathBuf::from(path)
}

/// True when any existing group file is at least as fresh as `since`.
async fn any_fresh(paths: &[PathBuf], since: SystemTime) -> bool {
    for path in paths {
        let fresh = tokio::fs::metadata(path)
            .await
            .and_then(|m| m.modified())
            .map(|mtime| mtime >= since)
            .unwrap_or(false);
        if fresh {
            return true;
        }
    }
    false
}

// ---- reference closure ----

/// Most directories one closure walk expands before giving up and keeping
/// all of CAS. Mirrors the `GetTree` traversal cap.
const MAX_CLOSURE_DIRS: usize = 100_000;

/// Every CAS content hash reachable from the kept action results: their file
/// outputs, log blobs, and output trees (expanded transitively). Returns
/// `None` when a tree exceeds the traversal cap: the caller then keeps all
/// of CAS rather than risk orphaning a result it could not fully walk.
/// Missing or corrupt blobs end that branch (pre-existing damage the prune
/// must not compound, and cannot repair).
async fn collect_referenced_cas(
    root: &Path,
    kept_ac: &[(DigestFunction, String)],
) -> std::io::Result<Option<HashSet<String>>> {
    let mut pinned: HashSet<String> = HashSet::new();
    let mut queue: Vec<reapi::Digest> = Vec::new();
    for (function, hash) in kept_ac {
        let path = ac_data_path(root, *function, hash);
        let Ok(bytes) = tokio::fs::read(&path).await else {
            continue;
        };
        let Ok(result) = reapi::ActionResult::decode(&bytes[..]) else {
            continue;
        };
        for digest in [&result.stdout_digest, &result.stderr_digest]
            .into_iter()
            .flatten()
        {
            pin_blob(root, digest, &mut pinned).await?;
        }
        for file in &result.output_files {
            if let Some(digest) = &file.digest {
                pin_blob(root, digest, &mut pinned).await?;
            }
        }
        for dir in &result.output_directories {
            // The tree blob describes the whole subtree in one read.
            if let Some(tree_digest) = &dir.tree_digest
                && let Some(bytes) = read_cas_blob(root, tree_digest).await?
            {
                pin_hash(&mut pinned, &tree_digest.hash);
                if let Ok(tree) = reapi::Tree::decode(&bytes[..]) {
                    if let Some(root) = &tree.root {
                        expand_directory(root, &mut pinned, &mut queue);
                    }
                    for child in &tree.children {
                        expand_directory(child, &mut pinned, &mut queue);
                    }
                    continue;
                }
            }
            // No (or corrupt) tree blob: walk from the root directory.
            if let Some(digest) = &dir.root_directory_digest {
                queue.push(digest.clone());
            }
        }
    }
    let mut seen: HashSet<(String, i64)> = HashSet::new();
    while let Some(digest) = queue.pop() {
        if !seen.insert((digest.hash.clone(), digest.size_bytes)) {
            continue;
        }
        if seen.len() > MAX_CLOSURE_DIRS {
            tracing::warn!("prune closure exceeded its traversal cap; keeping all of CAS");
            return Ok(None);
        }
        let Some(bytes) = read_cas_blob(root, &digest).await? else {
            continue;
        };
        let Ok(directory) = reapi::Directory::decode(&bytes[..]) else {
            continue;
        };
        // The directory blob itself is referenced by its parent: pin it
        // before expanding, or the walk orphans its own trail.
        pin_hash(&mut pinned, &digest.hash);
        expand_directory(&directory, &mut pinned, &mut queue);
    }
    Ok(Some(pinned))
}

/// Pin one blob digest and confirm which function dir holds it, so later
/// readers know the bytes exist. Missing blobs pin nothing.
async fn pin_blob(
    root: &Path,
    digest: &reapi::Digest,
    pinned: &mut HashSet<String>,
) -> std::io::Result<()> {
    if read_cas_blob(root, digest).await?.is_some() {
        pin_hash(pinned, &digest.hash);
    }
    Ok(())
}

fn pin_hash(pinned: &mut HashSet<String>, hash: &str) {
    if let Some(normalized) = normalize_hash(hash) {
        pinned.insert(normalized);
    }
}

/// Read one CAS blob, fanning out across candidate functions by hex length
/// exactly like a store lookup. Size-mismatched candidates are skipped;
/// `None` means no candidate holds the blob.
async fn read_cas_blob(root: &Path, digest: &reapi::Digest) -> std::io::Result<Option<Vec<u8>>> {
    if digest.size_bytes < 0 {
        return Ok(None);
    }
    let Some(hash) = normalize_hash(&digest.hash) else {
        return Ok(None);
    };
    for function in DigestFunction::by_hex_len(hash.len()) {
        let path = root
            .join("cas")
            .join(function.dir_name())
            .join(&hash[..2.min(hash.len())])
            .join(&hash);
        let Ok(bytes) = tokio::fs::read(&path).await else {
            continue;
        };
        if bytes.len() as u64 == digest.size_bytes as u64 {
            return Ok(Some(bytes));
        }
    }
    Ok(None)
}

/// Pin a directory's file blobs and queue its subdirectories.
fn expand_directory(
    directory: &reapi::Directory,
    pinned: &mut HashSet<String>,
    queue: &mut Vec<reapi::Digest>,
) {
    for file in &directory.files {
        if let Some(digest) = &file.digest {
            pin_hash(pinned, &digest.hash);
        }
    }
    for dir in &directory.directories {
        if let Some(digest) = &dir.digest {
            queue.push(digest.clone());
        }
    }
}

fn ac_data_path(root: &Path, function: DigestFunction, hash: &str) -> PathBuf {
    root.join("ac")
        .join(function.dir_name())
        .join(&hash[..2.min(hash.len())])
        .join(hash)
}

// ---- deletion ----

/// Delete one file, accounting its bytes. Missing files are fine (a group
/// delete may have taken the companion already); other errors fail loudly
/// rather than silently keeping garbage in the uploaded cache.
async fn remove_file(
    path: &Path,
    dry_run: bool,
    stats: &mut PruneStats,
) -> std::io::Result<()> {
    let bytes = tokio::fs::metadata(path)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    if dry_run {
        if tokio::fs::try_exists(path).await.unwrap_or(false) {
            stats.freed_bytes += bytes;
        }
        return Ok(());
    }
    match tokio::fs::remove_file(path).await {
        Ok(()) => {
            stats.freed_bytes += bytes;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::DigestFunction;
    use crate::store::Hints;
    use crate::store::Store;

    fn test_root(name: &str) -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("reapid-test-{}-{}-prune", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn backdate(path: &Path, secs: u64) {
        let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(old)
            .unwrap();
    }

    /// Backdate every on-disk file of one CAS entry (data, sidecar,
    /// compressed form) so the group reads as untouched since `since`.
    fn backdate_group(store: &Store, hash: &str, secs: u64) {
        let data = store.cas_data_path(DigestFunction::Sha256, hash);
        backdate(&data, secs);
        let mut meta = data.as_os_str().to_owned();
        meta.push(".meta.json");
        let meta = std::path::PathBuf::from(meta);
        if meta.exists() {
            backdate(&meta, secs);
        }
        let mut zst = data.as_os_str().to_owned();
        zst.push(".zst");
        let zst = std::path::PathBuf::from(zst);
        if zst.exists() {
            backdate(&zst, secs);
        }
    }

    fn backdate_ac(store: &Store, hash: &str, secs: u64) {
        let data = store.ac_data_path(DigestFunction::Sha256, hash);
        backdate(&data, secs);
        let mut meta = data.as_os_str().to_owned();
        meta.push(".meta.json");
        backdate(&std::path::PathBuf::from(meta), secs);
    }

    fn action_result_with_outputs(
        files: &[(&str, &reapi::Digest)],
        dirs: &[(&str, &reapi::Digest, &reapi::Digest)],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        reapi::ActionResult {
            output_files: files
                .iter()
                .map(|(path, digest)| reapi::OutputFile {
                    path: path.to_string(),
                    digest: Some((*digest).clone()),
                    ..Default::default()
                })
                .collect(),
            output_directories: dirs
                .iter()
                .map(|(path, tree, root)| reapi::OutputDirectory {
                    path: path.to_string(),
                    tree_digest: Some((*tree).clone()),
                    root_directory_digest: Some((*root).clone()),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
        .encode(&mut bytes)
        .unwrap();
        bytes
    }

    #[tokio::test]
    async fn keeps_touched_removes_stale() {
        let root = test_root("keep-stale");
        let old_hash;
        let new_hash;
        {
            let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
            let old = b"stale blob";
            old_hash = DigestFunction::Sha256.hexdigest(old);
            store
                .cas_put(old, &old_hash, old.len() as u64, 0, &Hints::default(), 1000)
                .await
                .unwrap();
            let new = b"fresh blob";
            new_hash = DigestFunction::Sha256.hexdigest(new);
            store
                .cas_put(new, &new_hash, new.len() as u64, 0, &Hints::default(), 1000)
                .await
                .unwrap();
            // A read is a touch: the sidecar rewrite keeps this entry.
            store
                .record_access(
                    &crate::store::EntryKey {
                        kind: crate::store::EntryKind::Cas,
                        function: DigestFunction::Sha256,
                        hash: new_hash.clone(),
                    },
                    2000,
                )
                .await
                .unwrap();
            backdate_group(&store, &old_hash, 500);
        }
        let since = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1500);
        let stats = prune_untouched(&root, since, false).await.unwrap();
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.kept, 1);
        assert_eq!(stats.removed, 1);
        assert!(stats.freed_bytes > 0);
        // The stale entry is gone entirely: data, sidecar, everything.
        let store = Store::open(root.clone(), 1 << 30, 3000).await.unwrap();
        assert!(store.cas_lookup(&old_hash, 0).is_none());
        assert!(store.cas_lookup(&new_hash, 0).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn dry_run_reports_the_real_outcome() {
        let root = test_root("dry-equals-real");
        {
            let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
            for name in [b"old one".as_slice(), b"old two".as_slice(), b"new".as_slice()] {
                let hash = DigestFunction::Sha256.hexdigest(name);
                store
                    .cas_put(name, &hash, name.len() as u64, 0, &Hints::default(), 1000)
                    .await
                    .unwrap();
                if name != b"new" {
                    backdate_group(&store, &hash, 500);
                }
            }
        }
        let since = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1500);
        let dry = prune_untouched(&root, since, true).await.unwrap();
        assert_eq!((dry.scanned, dry.kept, dry.removed), (3, 1, 2));
        let real = prune_untouched(&root, since, false).await.unwrap();
        // The dry run must neither miss nor double-count (e.g. recounting a
        // deleted group's sidecar in the orphan sweep).
        assert_eq!(dry, real);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn closure_keeps_blobs_referenced_by_kept_results() {
        let root = test_root("closure");
        let output_hash;
        let nested_hash;
        let action_hash;
        {
            let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
            // A file output plus a directory tree: root dir -> subdir ->
            // nested file. Every blob backdated stale; only the action
            // result stays fresh.
            let output = b"stale output";
            output_hash = DigestFunction::Sha256.hexdigest(output);
            store
                .cas_put(output, &output_hash, output.len() as u64, 0, &Hints::default(), 1000)
                .await
                .unwrap();
            let nested = b"stale nested";
            nested_hash = DigestFunction::Sha256.hexdigest(nested);
            store
                .cas_put(nested, &nested_hash, nested.len() as u64, 0, &Hints::default(), 1000)
                .await
                .unwrap();
            let subdir = reapi::Directory {
                files: vec![reapi::FileNode {
                    name: "nested".to_string(),
                    digest: Some(reapi::Digest {
                        hash: nested_hash.clone(),
                        size_bytes: nested.len() as i64,
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            };
            let mut subdir_bytes = Vec::new();
            subdir.encode(&mut subdir_bytes).unwrap();
            let subdir_hash = DigestFunction::Sha256.hexdigest(&subdir_bytes);
            store
                .cas_put(
                    &subdir_bytes,
                    &subdir_hash,
                    subdir_bytes.len() as u64,
                    0,
                    &Hints::default(),
                    1000,
                )
                .await
                .unwrap();
            let tree = reapi::Tree {
                root: Some(reapi::Directory {
                    directories: vec![reapi::DirectoryNode {
                        name: "sub".to_string(),
                        digest: Some(reapi::Digest {
                            hash: subdir_hash.clone(),
                            size_bytes: subdir_bytes.len() as i64,
                        }),
                    }],
                    ..Default::default()
                }),
                children: vec![subdir],
            };
            let mut tree_bytes = Vec::new();
            tree.encode(&mut tree_bytes).unwrap();
            let tree_hash = DigestFunction::Sha256.hexdigest(&tree_bytes);
            store
                .cas_put(
                    &tree_bytes,
                    &tree_hash,
                    tree_bytes.len() as u64,
                    0,
                    &Hints::default(),
                    1000,
                )
                .await
                .unwrap();
            let action = b"action";
            action_hash = DigestFunction::Sha256.hexdigest(action);
            let output_digest = reapi::Digest {
                hash: output_hash.clone(),
                size_bytes: output.len() as i64,
            };
            let tree_digest = reapi::Digest {
                hash: tree_hash.clone(),
                size_bytes: tree_bytes.len() as i64,
            };
            let mut root_bytes = Vec::new();
            tree.root.clone().unwrap().encode(&mut root_bytes).unwrap();
            let root_digest = reapi::Digest {
                hash: DigestFunction::Sha256.hexdigest(&root_bytes),
                size_bytes: root_bytes.len() as i64,
            };
            let result = action_result_with_outputs(
                &[("out", &output_digest)],
                &[("dir", &tree_digest, &root_digest)],
            );
            store
                .ac_put(&action_hash, 0, &result, &Hints::default(), 1000)
                .await
                .unwrap();
            // Everything stale except the action result: backdate every CAS
            // blob, then re-touch the result through a read.
            for hash in [&output_hash, &nested_hash, &subdir_hash, &tree_hash] {
                backdate_group(&store, hash, 500);
            }
            store
                .record_access(
                    &crate::store::EntryKey {
                        kind: crate::store::EntryKind::ActionCache,
                        function: DigestFunction::Sha256,
                        hash: action_hash.clone(),
                    },
                    2000,
                )
                .await
                .unwrap();
        }
        let since = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1500);
        let stats = prune_untouched(&root, since, false).await.unwrap();
        // The result plus its file output, nested blob, subdir, and tree
        // all survive although every CAS blob reads stale.
        assert_eq!(stats.removed, 0);
        let store = Store::open(root.clone(), 1 << 30, 3000).await.unwrap();
        assert!(store.ac_lookup(&action_hash, 0).is_some());
        for hash in [&output_hash, &nested_hash] {
            assert!(store.cas_lookup(hash, 0).is_some(), "{hash} was swept");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn unreferenced_stale_blobs_go_with_stale_results() {
        let root = test_root("unreferenced");
        let output_hash;
        let action_hash;
        {
            let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
            let output = b"orphaned output";
            output_hash = DigestFunction::Sha256.hexdigest(output);
            store
                .cas_put(output, &output_hash, output.len() as u64, 0, &Hints::default(), 1000)
                .await
                .unwrap();
            let action = b"stale action";
            action_hash = DigestFunction::Sha256.hexdigest(action);
            let result = action_result_with_outputs(
                &[(
                    "out",
                    &reapi::Digest {
                        hash: output_hash.clone(),
                        size_bytes: output.len() as i64,
                    },
                )],
                &[],
            );
            store
                .ac_put(&action_hash, 0, &result, &Hints::default(), 1000)
                .await
                .unwrap();
            // Both the result and its output read stale: the pair goes.
            backdate_group(&store, &output_hash, 500);
            backdate_ac(&store, &action_hash, 500);
        }
        let since = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1500);
        let stats = prune_untouched(&root, since, false).await.unwrap();
        assert_eq!((stats.scanned, stats.kept, stats.removed), (2, 0, 2));
        let store = Store::open(root.clone(), 1 << 30, 3000).await.unwrap();
        assert!(store.ac_lookup(&action_hash, 0).is_none());
        assert!(store.cas_lookup(&output_hash, 0).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn sweeps_debris_uploads_and_orphans() {
        let root = test_root("sweep");
        // Capture `since` first: everything created below reads as touched.
        let since = std::time::SystemTime::now();
        let hash;
        {
            let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
            let data = b"kept blob";
            hash = DigestFunction::Sha256.hexdigest(data);
            store
                .cas_put(data, &hash, data.len() as u64, 0, &Hints::default(), 1000)
                .await
                .unwrap();
            // Crash debris, a stale partial upload, and an orphan sidecar.
            let shard = store
                .cas_data_path(DigestFunction::Sha256, &hash)
                .parent()
                .unwrap()
                .to_path_buf();
            tokio::fs::write(shard.join("abc.tmp-1-2"), b"partial").await.unwrap();
            tokio::fs::write(root.join("uploads").join("uuid.part"), b"partial")
                .await
                .unwrap();
            tokio::fs::write(shard.join("00deadbeef.meta.json"), b"{}").await.unwrap();
        }
        let stats = prune_untouched(&root, since, false).await.unwrap();
        // The fresh entry survives (data mtime is now); everything else goes.
        assert_eq!(stats.kept, 1);
        assert_eq!(stats.uploads_cleared, 1);
        assert!(stats.freed_bytes > 0);
        let store = Store::open(root.clone(), 1 << 30, 3000).await.unwrap();
        assert!(store.cas_lookup(&hash, 0).is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn touch_refreshes_data_mtime() {
        let root = test_root("touch-mtime");
        let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
        let data = b"touched blob";
        let hash = DigestFunction::Sha256.hexdigest(data);
        store
            .cas_put(data, &hash, data.len() as u64, 0, &Hints::default(), 1000)
            .await
            .unwrap();
        let path = store.cas_data_path(DigestFunction::Sha256, &hash);
        backdate(&path, 500);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        store.touch(
            &crate::store::EntryKey {
                kind: crate::store::EntryKind::Cas,
                function: DigestFunction::Sha256,
                hash: hash.clone(),
            },
            2000,
        );
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(after > before);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn birth_round_trips() {
        let root = test_root("birth");
        {
            let store = Store::open(root.clone(), 1 << 30, 4242).await.unwrap();
            assert_eq!(store.birth(), 4242);
            assert_eq!(crate::store::read_birth(&root).unwrap(), 4242);
        }
        // Reopening overwrites the marker with the most recent process.
        let store = Store::open(root.clone(), 1 << 30, 5000).await.unwrap();
        assert_eq!(store.birth(), 5000);
        assert_eq!(crate::store::read_birth(&root).unwrap(), 5000);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn missing_birth_is_an_error() {
        let root = test_root("no-birth");
        std::fs::create_dir_all(&root).unwrap();
        assert!(crate::store::read_birth(&root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
