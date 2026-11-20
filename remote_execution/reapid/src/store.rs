//! Directory-backed blob store.
//!
//! Layout under the store root (all paths content-addressed):
//!
//! ```text
//! cas/<function>/<aa>/<hash>          canonical (uncompressed) bytes
//! cas/<function>/<aa>/<hash>.zst      as-received zstd bytes, when uploaded compressed
//!                                     (or encoded on demand for a compressed read)
//! cas/<function>/<aa>/<hash>.meta.json  sidecar: sizes, times, request hints
//! ac/<function>/<aa>/<hash>           prost-encoded ActionResult
//! ac/<function>/<aa>/<hash>.meta.json
//! uploads/<uuid>                      in-progress ByteStream writes (wiped on open)
//! ```
//!
//! Every blob is verified against its digest before the atomic rename into
//! place, so bytes on disk are trusted without re-hashing. An in-memory
//! index (rebuilt by scan on open) tracks sizes and metadata for the
//! evictor; the sidecars make it crash-safe. Every committing rename is
//! fsynced (file before, directory after), so acknowledged writes survive
//! power loss, not just clean restarts.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use serde::Deserialize;
use serde::Serialize;

use crate::digest::DigestFunction;
use crate::digest::Hasher;
use crate::digest::ResolveError;
use crate::digest::normalize_hash;
use crate::digest::resolve;

/// Which pool an entry lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum EntryKind {
    Cas,
    ActionCache,
}

impl EntryKind {
    fn dir_name(self) -> &'static str {
        match self {
            EntryKind::Cas => "cas",
            EntryKind::ActionCache => "ac",
        }
    }
}

/// Identity of one stored entry.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct EntryKey {
    pub kind: EntryKind,
    pub function: DigestFunction,
    pub hash: String,
}

/// Identifying metadata captured from the request that stored (or last
/// touched) the blob. CAS blobs are shared across actions, so these are
/// last-writer-wins hints for cache policy matching, not provenance.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_invocation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correlated_invocations_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mnemonic: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_id: Option<String>,
}

impl Hints {
    pub fn is_empty(&self) -> bool {
        self.action_id.is_none()
            && self.tool_invocation_id.is_none()
            && self.correlated_invocations_id.is_none()
            && self.mnemonic.is_none()
            && self.target_id.is_none()
            && self.configuration_id.is_none()
    }

    /// Overlay non-empty `other` fields onto `self`.
    pub fn overlay(&mut self, other: &Hints) {
        macro_rules! pick {
            ($field:ident) => {
                if self.$field.is_none() {
                    self.$field = other.$field.clone();
                }
            };
        }
        pick!(action_id);
        pick!(tool_invocation_id);
        pick!(correlated_invocations_id);
        pick!(mnemonic);
        pick!(target_id);
        pick!(configuration_id);
    }
}

/// Sidecar record for one entry. Times are unix seconds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobMeta {
    pub size_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compressed_size_bytes: Option<u64>,
    pub first_seen: u64,
    pub last_access: u64,
    pub access_count: u64,
    /// Monotonic recency sequence: breaks `last_access` ties (bulk uploads
    /// share a second) so LRU order matches access order. Persisted, so a
    /// restart keeps the order; the counter reseeds from the max on scan.
    #[serde(default)]
    pub seq: u64,
    #[serde(flatten)]
    pub hints: Hints,
}

impl BlobMeta {
    fn new(size_bytes: u64, now: u64, hints: Hints) -> BlobMeta {
        BlobMeta {
            size_bytes,
            compressed_size_bytes: None,
            first_seen: now,
            last_access: now,
            access_count: 0,
            seq: 0,
            hints,
        }
    }

    /// Disk footprint of the entry (canonical bytes + stored compressed form).
    pub fn footprint(&self) -> u64 {
        self.size_bytes + self.compressed_size_bytes.unwrap_or(0)
    }
}

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Resolve(ResolveError),
    SizeMismatch { expected: u64, actual: u64 },
    TooLarge { size: u64, max: u64 },
    Decode(String),
    UploadNotFound,
    UploadOffset { expected: u64, actual: u64 },
    UploadFinished,
    UploadMismatch,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::Io(e) => write!(f, "store i/o: {e}"),
            StoreError::Resolve(e) => write!(f, "{e}"),
            StoreError::SizeMismatch { expected, actual } => {
                write!(f, "size mismatch: digest says {expected} bytes, got {actual}")
            }
            StoreError::TooLarge { size, max } => {
                write!(f, "blob size {size} exceeds limit {max}")
            }
            StoreError::Decode(e) => write!(f, "zstd decode failed: {e}"),
            StoreError::UploadNotFound => write!(f, "unknown upload"),
            StoreError::UploadOffset { expected, actual } => {
                write!(f, "upload offset {actual}, expected {expected}")
            }
            StoreError::UploadFinished => write!(f, "write after finish"),
            StoreError::UploadMismatch => {
                write!(f, "resumed upload describes a different blob")
            }
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> StoreError {
        StoreError::Io(e)
    }
}

/// Expected identity of an in-progress upload.
#[derive(Clone, Debug)]
pub struct UploadExpect {
    pub hash: String,
    pub size_bytes: u64,
    /// Explicit digest function if the client stated one (0 = infer at finish).
    pub proto_function: i32,
    pub compressed: bool,
}

struct Upload {
    path: PathBuf,
    committed: u64,
    expect: UploadExpect,
    hints: Hints,
    /// Set when finish/abort claims the upload: appends racing the claim
    /// fail instead of writing to a renamed-away spool.
    sealed: bool,
}

pub struct Store {
    root: PathBuf,
    max_blob_bytes: u64,
    index: Mutex<HashMap<EntryKey, BlobMeta>>,
    /// In-progress uploads, each behind its own async mutex so concurrent
    /// appends to one id serialize (check + write + account as one unit)
    /// without serializing unrelated uploads. Lock order is map-then-upload,
    /// and the map lock is never held across an await, so finish/abort may
    /// take the map lock while holding an upload lock without deadlocking.
    uploads: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Upload>>>>,
    tmp_counter: AtomicU64,
    seq: AtomicU64,
}

impl Store {
    /// Open (creating) the store at `root`, wipe stale partial uploads, and
    /// rebuild the index by scanning the tree. `now` seeds records that must
    /// be reconstructed (missing/corrupt sidecars).
    pub async fn open(root: PathBuf, max_blob_bytes: u64, now: u64) -> Result<Store, StoreError> {
        for dir in ["cas", "ac", "uploads"] {
            tokio::fs::create_dir_all(root.join(dir)).await?;
        }
        // Abandoned partial uploads never resume across restarts; drop them so
        // a crash cannot leak disk or confuse a retried upload id.
        let uploads_dir = root.join("uploads");
        let mut entries = tokio::fs::read_dir(&uploads_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
        let store = Store {
            root,
            max_blob_bytes,
            index: Mutex::new(HashMap::new()),
            uploads: Mutex::new(HashMap::new()),
            tmp_counter: AtomicU64::new(0),
            seq: AtomicU64::new(0),
        };
        store.scan(now).await?;
        Ok(store)
    }

    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    fn shard(hash: &str) -> &str {
        &hash[..2.min(hash.len())]
    }

    fn data_path(&self, kind: EntryKind, function: DigestFunction, hash: &str) -> PathBuf {
        self.root
            .join(kind.dir_name())
            .join(function.dir_name())
            .join(Self::shard(hash))
            .join(hash)
    }

    fn cas_compressed_path(&self, function: DigestFunction, hash: &str) -> PathBuf {
        let mut path = self.data_path(EntryKind::Cas, function, hash).into_os_string();
        path.push(".zst");
        PathBuf::from(path)
    }

    fn meta_path_for(data_path: &std::path::Path) -> PathBuf {
        let mut path = data_path.as_os_str().to_owned();
        path.push(".meta.json");
        PathBuf::from(path)
    }

    fn lock_index(&self) -> std::sync::MutexGuard<'_, HashMap<EntryKey, BlobMeta>> {
        self.index.lock().expect("index mutex poisoned")
    }

    fn lock_uploads(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<tokio::sync::Mutex<Upload>>>> {
        self.uploads.lock().expect("uploads mutex poisoned")
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    // ---- CAS writes ----

    /// Store uncompressed `data` as the CAS blob `(hash, size)`, verifying
    /// the digest first. Idempotent: re-storing identical bytes only refreshes
    /// metadata. Returns the resolved digest function.
    pub async fn cas_put(
        &self,
        data: &[u8],
        expected_hash: &str,
        expected_size: u64,
        proto_function: i32,
        hints: &Hints,
        now: u64,
    ) -> Result<DigestFunction, StoreError> {
        if data.len() as u64 != expected_size {
            return Err(StoreError::SizeMismatch {
                expected: expected_size,
                actual: data.len() as u64,
            });
        }
        if expected_size > self.max_blob_bytes {
            return Err(StoreError::TooLarge {
                size: expected_size,
                max: self.max_blob_bytes,
            });
        }
        let function = resolve(data, proto_function, expected_hash).map_err(StoreError::Resolve)?;
        let hash = normalize_hash(expected_hash).expect("resolve validated the hash");
        self.write_canonical(EntryKind::Cas, function, &hash, data, hints, now)
            .await?;
        Ok(function)
    }

    /// Store zstd-compressed `data` for the blob `(hash, size)`, where `size`
    /// is the *uncompressed* size. Decodes (capped at `size`, so a
    /// decompression bomb fails closed), verifies the digest of the decoded
    /// bytes, then stores both the canonical bytes and the as-received
    /// compressed form for passthrough reads.
    pub async fn cas_put_compressed(
        &self,
        data: &[u8],
        expected_hash: &str,
        expected_size: u64,
        proto_function: i32,
        hints: &Hints,
        now: u64,
    ) -> Result<DigestFunction, StoreError> {
        if expected_size > self.max_blob_bytes {
            return Err(StoreError::TooLarge {
                size: expected_size,
                max: self.max_blob_bytes,
            });
        }
        let decoded = decode_capped(data, expected_size, self.max_blob_bytes)?;
        if decoded.len() as u64 != expected_size {
            return Err(StoreError::SizeMismatch {
                expected: expected_size,
                actual: decoded.len() as u64,
            });
        }
        let function =
            resolve(&decoded, proto_function, expected_hash).map_err(StoreError::Resolve)?;
        let hash = normalize_hash(expected_hash).expect("resolve validated the hash");
        self.write_canonical(EntryKind::Cas, function, &hash, &decoded, hints, now)
            .await?;
        // Keep the sender's exact bytes for compressed reads (no re-encode).
        let zst_path = self.cas_compressed_path(function, &hash);
        self.write_file_atomic(&zst_path, data).await?;
        self.set_compressed_size(
            &EntryKey {
                kind: EntryKind::Cas,
                function,
                hash,
            },
            data.len() as u64,
            now,
        )
        .await?;
        Ok(function)
    }

    async fn write_canonical(
        &self,
        kind: EntryKind,
        function: DigestFunction,
        hash: &str,
        data: &[u8],
        hints: &Hints,
        now: u64,
    ) -> Result<(), StoreError> {
        let path = self.data_path(kind, function, hash);
        let key = EntryKey {
            kind,
            function,
            hash: hash.to_string(),
        };
        // NOTE: index guards never cross an await (std MutexGuard is !Send,
        // and these futures run on the multi-threaded runtime); every locked
        // section below is lexically scoped to end before the next await.
        let already_indexed = self
            .lock_index()
            .get(&key)
            .filter(|m| m.size_bytes == data.len() as u64)
            .is_some();
        if already_indexed && tokio::fs::try_exists(&path).await.unwrap_or(false) {
            // Already stored and verified in the past; just refresh hints.
            self.merge_hints(&key, hints, now).await?;
            return Ok(());
        }
        self.write_file_atomic(&path, data).await?;
        let mut meta = BlobMeta::new(data.len() as u64, now, hints.clone());
        meta.seq = self.next_seq();
        // Preserve any compressed form + first-seen time across re-uploads.
        let preserved = self.lock_index().get(&key).map(|existing| {
            (
                existing.first_seen,
                existing.access_count,
                existing.compressed_size_bytes,
                existing.hints.clone(),
            )
        });
        if let Some((first_seen, access_count, compressed_size_bytes, hints)) = preserved {
            meta.first_seen = first_seen;
            meta.access_count = access_count;
            meta.compressed_size_bytes = compressed_size_bytes;
            meta.hints.overlay(&hints);
        }
        self.write_meta_file(&key, &meta).await?;
        self.lock_index().insert(key, meta);
        Ok(())
    }

    async fn write_file_atomic(&self, path: &std::path::Path, data: &[u8]) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), n));
        // Durability, not just atomicity: the bytes must reach the disk
        // before the rename, and the rename before the write is
        // acknowledged, or a power cut can lose an acknowledged blob (or
        // resurrect it with torn bytes the scan would then trust).
        {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::File::create(&tmp).await?;
            file.write_all(data).await?;
            file.sync_all().await?;
        }
        tokio::fs::rename(&tmp, path).await?;
        sync_parent_dir(path).await?;
        Ok(())
    }

    async fn write_meta_file(&self, key: &EntryKey, meta: &BlobMeta) -> Result<(), StoreError> {
        let data_path = self.data_path(key.kind, key.function, &key.hash);
        let meta_path = Self::meta_path_for(&data_path);
        let json = serde_json::to_vec_pretty(meta).expect("meta serializes");
        self.write_file_atomic(&meta_path, &json).await
    }

    /// Apply `update` to the indexed meta for `key`; when it reports a change,
    /// persist the sidecar and store the result. Missing entries are a no-op.
    async fn update_meta(
        &self,
        key: &EntryKey,
        update: impl FnOnce(&mut BlobMeta) -> bool,
    ) -> Result<(), StoreError> {
        let updated = {
            let mut index = self.lock_index();
            match index.get_mut(key) {
                Some(meta) => {
                    if !update(meta) {
                        return Ok(());
                    }
                    Some(meta.clone())
                }
                None => None,
            }
        };
        if let Some(meta) = updated {
            self.write_meta_file(key, &meta).await?;
            self.lock_index().insert(key.clone(), meta);
        }
        Ok(())
    }

    async fn merge_hints(&self, key: &EntryKey, hints: &Hints, now: u64) -> Result<(), StoreError> {
        if hints.is_empty() {
            return Ok(());
        }
        self.update_meta(key, |meta| {
            let before = meta.hints.clone();
            meta.hints.overlay(hints);
            if meta.hints == before {
                return false;
            }
            meta.last_access = now;
            true
        })
        .await
    }

    async fn set_compressed_size(
        &self,
        key: &EntryKey,
        compressed_size: u64,
        now: u64,
    ) -> Result<(), StoreError> {
        self.update_meta(key, |meta| {
            meta.compressed_size_bytes = Some(compressed_size);
            meta.last_access = now;
            true
        })
        .await
    }

    // ---- CAS reads ----

    /// Look up a CAS blob across digest-function candidates (explicit function
    /// first when stated). Returns the stored function and a meta snapshot.
    /// The conventional empty blob synthesizes as present without storage.
    pub fn cas_lookup(&self, hash: &str, proto_function: i32) -> Option<(DigestFunction, BlobMeta)> {
        self.lookup(EntryKind::Cas, hash, proto_function)
    }

    /// Look up an entry, fanning out across candidate functions like CAS.
    fn lookup(
        &self,
        kind: EntryKind,
        hash: &str,
        proto_function: i32,
    ) -> Option<(DigestFunction, BlobMeta)> {
        let hash = normalize_hash(hash)?;
        if kind == EntryKind::Cas
            && let Some(empty_function) = empty_blob_function(&hash)
        {
            return Some((
                empty_function,
                BlobMeta {
                    size_bytes: 0,
                    compressed_size_bytes: None,
                    first_seen: 0,
                    last_access: 0,
                    access_count: 0,
                    seq: 0,
                    hints: Hints::default(),
                },
            ));
        }
        let index = self.lock_index();
        if proto_function != 0
            && let Some(function) = DigestFunction::from_proto(proto_function)
        {
            let key = EntryKey {
                kind,
                function,
                hash: hash.clone(),
            };
            if let Some(meta) = index.get(&key) {
                return Some((function, meta.clone()));
            }
        }
        for candidate in DigestFunction::by_hex_len(hash.len()) {
            let key = EntryKey {
                kind,
                function: *candidate,
                hash: hash.clone(),
            };
            if let Some(meta) = index.get(&key) {
                return Some((*candidate, meta.clone()));
            }
        }
        None
    }

    /// Filesystem path of canonical bytes for a looked-up blob.
    pub fn cas_data_path(&self, function: DigestFunction, hash: &str) -> PathBuf {
        self.data_path(EntryKind::Cas, function, hash)
    }

    /// Path of the stored zstd form, encoding on demand from the canonical
    /// bytes when absent. `None` only if the blob itself is unknown.
    pub async fn cas_compressed_path_for(
        &self,
        function: DigestFunction,
        hash: &str,
        now: u64,
    ) -> Result<Option<PathBuf>, StoreError> {
        let key = EntryKey {
            kind: EntryKind::Cas,
            function,
            hash: hash.to_string(),
        };
        let has_canonical = self.lock_index().contains_key(&key);
        if !has_canonical {
            return Ok(None);
        }
        let zst_path = self.cas_compressed_path(function, hash);
        if tokio::fs::try_exists(&zst_path).await.unwrap_or(false) {
            return Ok(Some(zst_path));
        }
        let data = tokio::fs::read(self.data_path(EntryKind::Cas, function, hash)).await?;
        let encoded =
            ruzstd::encoding::compress_to_vec(&data[..], ruzstd::encoding::CompressionLevel::Fastest);
        self.write_file_atomic(&zst_path, &encoded).await?;
        self.set_compressed_size(&key, encoded.len() as u64, now).await?;
        Ok(Some(zst_path))
    }

    /// Best-effort recency bump without persisting (for high-volume calls
    /// like FindMissingBlobs, where a sidecar rewrite per digest would be
    /// pure write amplification).
    pub fn touch(&self, key: &EntryKey, now: u64) {
        let seq = self.next_seq();
        if let Some(meta) = self.lock_index().get_mut(key) {
            meta.last_access = now;
            meta.access_count += 1;
            meta.seq = seq;
        }
    }

    /// Record a read: bump access stats and persist the sidecar.
    pub async fn record_access(&self, key: &EntryKey, now: u64) -> Result<(), StoreError> {
        let seq = self.next_seq();
        self.update_meta(key, |meta| {
            meta.last_access = now;
            meta.access_count += 1;
            meta.seq = seq;
            true
        })
        .await
    }

    // ---- ActionCache ----

    /// Store an ActionResult under the action digest. The action blob itself
    /// is not required to be present (plain cache semantics).
    pub async fn ac_put(
        &self,
        action_hash: &str,
        proto_function: i32,
        result_bytes: &[u8],
        hints: &Hints,
        now: u64,
    ) -> Result<DigestFunction, StoreError> {
        let Some(hash) = normalize_hash(action_hash) else {
            return Err(StoreError::Resolve(ResolveError::MalformedHash));
        };
        let function = match proto_function {
            0 => DigestFunction::by_hex_len(hash.len())
                .first()
                .copied()
                .ok_or(StoreError::Resolve(ResolveError::MalformedHash))?,
            v => DigestFunction::from_proto(v)
                .ok_or(StoreError::Resolve(ResolveError::UnsupportedFunction(v)))?,
        };
        if result_bytes.len() as u64 > self.max_blob_bytes {
            return Err(StoreError::TooLarge {
                size: result_bytes.len() as u64,
                max: self.max_blob_bytes,
            });
        }
        self.write_canonical(EntryKind::ActionCache, function, &hash, result_bytes, hints, now)
            .await?;
        Ok(function)
    }

    /// Look up an ActionResult, fanning out across candidate functions like CAS.
    pub fn ac_lookup(&self, action_hash: &str, proto_function: i32) -> Option<(DigestFunction, BlobMeta)> {
        self.lookup(EntryKind::ActionCache, action_hash, proto_function)
    }

    pub fn ac_data_path(&self, function: DigestFunction, hash: &str) -> PathBuf {
        self.data_path(EntryKind::ActionCache, function, hash)
    }

    // ---- uploads ----

    /// Begin (or resume) a ByteStream upload. Resuming an existing id keeps
    /// its committed prefix, but the resumed identity must match the
    /// original: same id for a different blob is a client bug, failed here
    /// rather than verified (confusingly) at finish.
    pub async fn upload_begin(
        &self,
        id: &str,
        expect: UploadExpect,
        hints: &Hints,
    ) -> Result<u64, StoreError> {
        if expect.size_bytes > self.max_blob_bytes {
            return Err(StoreError::TooLarge {
                size: expect.size_bytes,
                max: self.max_blob_bytes,
            });
        }
        let Some(hash) = normalize_hash(&expect.hash) else {
            return Err(StoreError::Resolve(ResolveError::MalformedHash));
        };
        let mut expect = expect;
        expect.hash = hash;
        loop {
            let existing = self.lock_uploads().get(id).cloned();
            if let Some(existing) = existing {
                let upload = existing.lock().await;
                if upload.sealed {
                    return Err(StoreError::UploadFinished);
                }
                if !same_blob(&upload.expect, &expect) {
                    return Err(StoreError::UploadMismatch);
                }
                return Ok(upload.committed);
            }
            // Absent: insert, unless a racing begin beat us (then resume).
            let mut uploads = self.lock_uploads();
            if uploads.contains_key(id) {
                continue;
            }
            let path = self.root.join("uploads").join(format!("{id}.part"));
            uploads.insert(
                id.to_string(),
                Arc::new(tokio::sync::Mutex::new(Upload {
                    path,
                    committed: 0,
                    expect: expect.clone(),
                    hints: hints.clone(),
                    sealed: false,
                })),
            );
            return Ok(0);
        }
    }

    /// Append one chunk; `offset` must equal the committed prefix. Returns the
    /// new committed size. The check, the write, and the accounting hold the
    /// upload's lock as one unit, so concurrent writers to one id serialize
    /// instead of interleaving bytes.
    pub async fn upload_append(
        &self,
        id: &str,
        offset: u64,
        chunk: &[u8],
    ) -> Result<u64, StoreError> {
        let upload = {
            let uploads = self.lock_uploads();
            match uploads.get(id) {
                Some(upload) => upload.clone(),
                None => return Err(StoreError::UploadNotFound),
            }
        };
        let mut upload = upload.lock().await;
        if upload.sealed {
            return Err(StoreError::UploadFinished);
        }
        if offset != upload.committed {
            return Err(StoreError::UploadOffset {
                expected: upload.committed,
                actual: offset,
            });
        }
        if !chunk.is_empty() {
            use tokio::io::AsyncWriteExt;
            let mut file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&upload.path)
                .await?;
            file.write_all(chunk).await?;
            file.flush().await?;
        }
        upload.committed += chunk.len() as u64;
        // Fail fast rather than spooling unbounded bytes. Uncompressed
        // uploads cannot outgrow the digest size; compressed uploads state
        // the *decoded* size, so the wire cap is that plus headroom (zstd
        // framing overhead is kilobytes at most).
        let wire_cap = match upload.expect.compressed {
            true => upload.expect.size_bytes.saturating_add(1 << 20),
            false => upload.expect.size_bytes,
        };
        if upload.committed > wire_cap {
            let committed = upload.committed;
            let path = upload.path.clone();
            upload.sealed = true;
            self.lock_uploads().remove(id);
            drop(upload);
            let _ = tokio::fs::remove_file(&path).await;
            return Err(StoreError::SizeMismatch {
                expected: wire_cap,
                actual: committed,
            });
        }
        Ok(upload.committed)
    }

    pub async fn upload_committed(&self, id: &str) -> Option<u64> {
        let upload = self.lock_uploads().get(id).cloned()?;
        let upload = upload.lock().await;
        (!upload.sealed).then_some(upload.committed)
    }

    /// Finish an upload: verify and commit to CAS. Consumes the upload id.
    /// Verification streams the spooled file (bounded memory); on success
    /// the spool is renamed into place (uncompressed) or kept as the stored
    /// compressed form alongside fresh canonical bytes. Returns the resolved
    /// function and the WIRE bytes committed (what the client sent, which is
    /// what `WriteResponse.committed_size` must echo — for compressed
    /// uploads this differs from the decoded size).
    pub async fn upload_finish(&self, id: &str, now: u64) -> Result<(DigestFunction, u64), StoreError> {
        // Seal under the upload's lock (waiting out any in-flight append),
        // then drop the id: appends racing the claim fail sealed instead of
        // writing to the renamed-away spool.
        let (path, expect, hints, wire_committed) = {
            let upload = match self.lock_uploads().get(id).cloned() {
                Some(upload) => upload,
                None => return Err(StoreError::UploadNotFound),
            };
            let mut upload = upload.lock().await;
            if upload.sealed {
                return Err(StoreError::UploadNotFound);
            }
            upload.sealed = true;
            (
                upload.path.clone(),
                upload.expect.clone(),
                upload.hints.clone(),
                upload.committed,
            )
        };
        self.lock_uploads().remove(id);
        let result = match expect.compressed {
            true => self.finish_compressed_upload(&path, &expect, &hints, now).await,
            false => self.finish_plain_upload(&path, &expect, &hints, now).await,
        };
        // The spool is consumed on success (renamed); on failure it must go.
        if result.is_err() {
            let _ = tokio::fs::remove_file(&path).await;
        }
        result.map(|(function, _verified_size)| (function, wire_committed))
    }

    /// Candidate functions for an upload whose bytes hash to `hash`: the
    /// explicit function when valid, else length inference.
    fn upload_candidates(expect: &UploadExpect) -> Result<Vec<DigestFunction>, StoreError> {
        if expect.proto_function != 0 {
            return match DigestFunction::from_proto(expect.proto_function) {
                Some(function) => Ok(vec![function]),
                None => Err(StoreError::Resolve(ResolveError::UnsupportedFunction(
                    expect.proto_function,
                ))),
            };
        }
        let Some(hash) = normalize_hash(&expect.hash) else {
            return Err(StoreError::Resolve(ResolveError::MalformedHash));
        };
        let candidates = DigestFunction::by_hex_len(hash.len()).to_vec();
        if candidates.is_empty() {
            return Err(StoreError::Resolve(ResolveError::MalformedHash));
        }
        Ok(candidates)
    }

    /// Stream `path` through one hasher per candidate, returning the first
    /// candidate whose digest matches `hash` plus the byte count. Bounded
    /// memory however large the file.
    async fn verify_file(
        path: &std::path::Path,
        candidates: Vec<DigestFunction>,
        hash: &str,
    ) -> Result<(Option<DigestFunction>, u64), StoreError> {
        use tokio::io::AsyncReadExt;
        let mut hashers: Vec<Hasher> = candidates.iter().map(|f| Hasher::new(*f)).collect();
        let mut file = tokio::fs::File::open(path).await?;
        let mut total = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            total += n as u64;
            for hasher in &mut hashers {
                hasher.update(&buf[..n]);
            }
        }
        for (candidate, hasher) in candidates.into_iter().zip(hashers) {
            if hasher.verify(hash) {
                return Ok((Some(candidate), total));
            }
        }
        Ok((None, total))
    }

    async fn finish_plain_upload(
        &self,
        spool: &std::path::Path,
        expect: &UploadExpect,
        hints: &Hints,
        now: u64,
    ) -> Result<(DigestFunction, u64), StoreError> {
        let candidates = Self::upload_candidates(expect)?;
        let hash = normalize_hash(&expect.hash).expect("candidates validated the hash");
        if !tokio::fs::try_exists(spool).await.unwrap_or(false) {
            // Zero-chunk upload of the empty blob: no spool file exists.
            return self.commit_empty_upload(expect, hints, now).await;
        }
        let (matched, total) = Self::verify_file(spool, candidates, &hash).await?;
        if total != expect.size_bytes {
            return Err(StoreError::SizeMismatch {
                expected: expect.size_bytes,
                actual: total,
            });
        }
        let Some(function) = matched else {
            return Err(StoreError::Resolve(ResolveError::Mismatch));
        };
        let dest = self.data_path(EntryKind::Cas, function, &hash);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // Identical bytes already stored: drop the spool, refresh hints.
        let key = EntryKey {
            kind: EntryKind::Cas,
            function,
            hash: hash.clone(),
        };
        let already = self.lock_index().contains_key(&key);
        if already {
            let _ = tokio::fs::remove_file(spool).await;
            self.merge_hints(&key, hints, now).await?;
        } else {
            // The spool was only flushed chunk by chunk; sync it before the
            // rename commits it, or power loss can promote torn bytes the
            // scan trusts without re-hashing.
            tokio::fs::File::open(spool).await?.sync_all().await?;
            tokio::fs::rename(spool, &dest).await?;
            sync_parent_dir(&dest).await?;
            let mut meta = BlobMeta::new(total, now, hints.clone());
            meta.seq = self.next_seq();
            self.write_meta_file(&key, &meta).await?;
            self.lock_index().insert(key, meta);
        }
        Ok((function, total))
    }

    async fn finish_compressed_upload(
        &self,
        spool: &std::path::Path,
        expect: &UploadExpect,
        hints: &Hints,
        now: u64,
    ) -> Result<(DigestFunction, u64), StoreError> {
        let candidates = Self::upload_candidates(expect)?;
        let hash = normalize_hash(&expect.hash).expect("candidates validated the hash");
        // Decode streams from the spool (blocking decode on a worker thread;
        // the canonical bytes land in a second spool, then both rename).
        let canonical_spool = spool.with_extension("decoded");
        let max_window = self.max_blob_bytes;
        let expected_size = expect.size_bytes;
        let spool_owned = spool.to_path_buf();
        let canonical_owned = canonical_spool.clone();
        let decoded_size = tokio::task::spawn_blocking(move || {
            stream_decode_to_file(&spool_owned, &canonical_owned, expected_size, max_window)
        })
        .await
        .map_err(|e| StoreError::Decode(format!("decode task failed: {e}")))??;
        // Hash-verify the decoded bytes.
        let (matched, _) = Self::verify_file(&canonical_spool, candidates, &hash).await?;
        let cleanup = || async {
            let _ = tokio::fs::remove_file(spool).await;
            let _ = tokio::fs::remove_file(&canonical_spool).await;
        };
        let Some(function) = matched else {
            cleanup().await;
            return Err(StoreError::Resolve(ResolveError::Mismatch));
        };
        let dest = self.data_path(EntryKind::Cas, function, &hash);
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let key = EntryKey {
            kind: EntryKind::Cas,
            function,
            hash: hash.clone(),
        };
        let already = self.lock_index().contains_key(&key);
        if already {
            cleanup().await;
            self.merge_hints(&key, hints, now).await?;
        } else {
            // Both spools were synced on their open handles inside
            // stream_decode_to_file; these renames only need the
            // directory sync after them.
            tokio::fs::rename(&canonical_spool, &dest).await?;
            let zst_dest = self.cas_compressed_path(function, &hash);
            tokio::fs::rename(spool, &zst_dest).await?;
            sync_parent_dir(&dest).await?;
            let zst_size = tokio::fs::metadata(&zst_dest).await.map(|m| m.len()).unwrap_or(0);
            let mut meta = BlobMeta::new(decoded_size, now, hints.clone());
            meta.seq = self.next_seq();
            meta.compressed_size_bytes = Some(zst_size);
            self.write_meta_file(&key, &meta).await?;
            self.lock_index().insert(key, meta);
        }
        Ok((function, decoded_size))
    }

    /// Zero-chunk upload (no spool file): only the empty blob can finish this
    /// way, and it synthesizes as present without storage.
    async fn commit_empty_upload(
        &self,
        expect: &UploadExpect,
        _hints: &Hints,
        _now: u64,
    ) -> Result<(DigestFunction, u64), StoreError> {
        if expect.size_bytes != 0 {
            return Err(StoreError::SizeMismatch {
                expected: expect.size_bytes,
                actual: 0,
            });
        }
        let candidates = Self::upload_candidates(expect)?;
        let hash = normalize_hash(&expect.hash).expect("candidates validated the hash");
        for candidate in candidates {
            if candidate.empty_hex() == hash {
                return Ok((candidate, 0));
            }
        }
        Err(StoreError::Resolve(ResolveError::Mismatch))
    }

    pub async fn upload_abort(&self, id: &str) {
        let path = {
            let upload = match self.lock_uploads().get(id).cloned() {
                Some(upload) => upload,
                None => return,
            };
            let mut upload = upload.lock().await;
            upload.sealed = true;
            upload.path.clone()
        };
        self.lock_uploads().remove(id);
        let _ = tokio::fs::remove_file(path).await;
    }

    /// Fresh id for direct `blobs/` writes, which carry no client uuid.
    pub fn fresh_upload_id(&self) -> String {
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        format!("inline-{}-{}", std::process::id(), n)
    }

    // ---- index / eviction support ----

    pub fn snapshot(&self) -> Vec<(EntryKey, BlobMeta)> {
        self.lock_index()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    pub fn total_bytes(&self) -> u64 {
        self.lock_index().values().map(BlobMeta::footprint).sum()
    }

    pub fn len(&self) -> usize {
        self.lock_index().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Delete one entry (bytes + compressed form + sidecar + index row).
    /// Missing files are fine; the index row is always dropped.
    pub async fn remove(&self, key: &EntryKey) {
        let data_path = self.data_path(key.kind, key.function, &key.hash);
        let _ = tokio::fs::remove_file(&data_path).await;
        if key.kind == EntryKind::Cas {
            let _ = tokio::fs::remove_file(self.cas_compressed_path(key.function, &key.hash)).await;
        }
        let _ = tokio::fs::remove_file(Self::meta_path_for(&data_path)).await;
        self.lock_index().remove(key);
    }

    /// Rebuild the index from disk, repairing what can be repaired: entries
    /// with missing/corrupt sidecars get reconstructed records (size from the
    /// file, no hints); sidecars without bytes, compressed forms without
    /// canonical bytes, and `*.tmp-*` debris from writes a crash interrupted
    /// mid-rename are deleted.
    async fn scan(&self, now: u64) -> Result<(), StoreError> {
        for kind in [EntryKind::Cas, EntryKind::ActionCache] {
            let kind_dir = self.root.join(kind.dir_name());
            let mut fn_dirs = match tokio::fs::read_dir(&kind_dir).await {
                Ok(d) => d,
                Err(_) => continue,
            };
            while let Some(fn_entry) = fn_dirs.next_entry().await? {
                let Some(function) = DigestFunction::from_dir_name(&fn_entry.file_name().to_string_lossy())
                else {
                    continue;
                };
                let mut shards = match tokio::fs::read_dir(fn_entry.path()).await {
                    Ok(d) => d,
                    Err(_) => continue,
                };
                while let Some(shard_entry) = shards.next_entry().await? {
                    let mut files = match tokio::fs::read_dir(shard_entry.path()).await {
                        Ok(d) => d,
                        Err(_) => continue,
                    };
                    while let Some(file_entry) = files.next_entry().await? {
                        let name = file_entry.file_name().to_string_lossy().into_owned();
                        // Crash debris and orphan companions are swept inline:
                        // content hashes are pure hex, so `.tmp-` in a name
                        // can only be our own debris from a write a crash
                        // interrupted between temp-file creation and rename.
                        if name.contains(".tmp-") {
                            let _ = tokio::fs::remove_file(file_entry.path()).await;
                            continue;
                        }
                        if let Some(base) = name.strip_suffix(".meta.json") {
                            if !tokio::fs::try_exists(shard_entry.path().join(base))
                                .await
                                .unwrap_or(false)
                            {
                                let _ = tokio::fs::remove_file(file_entry.path()).await;
                            }
                            continue;
                        }
                        if let Some(base) = name.strip_suffix(".zst") {
                            // Only CAS blobs have compressed forms; an
                            // ac/*.zst is always garbage.
                            let orphan = kind == EntryKind::ActionCache
                                || !tokio::fs::try_exists(shard_entry.path().join(base))
                                    .await
                                    .unwrap_or(false);
                            if orphan {
                                let _ = tokio::fs::remove_file(file_entry.path()).await;
                            }
                            continue;
                        }
                        if normalize_hash(&name).is_none() {
                            continue;
                        }
                        let key = EntryKey {
                            kind,
                            function,
                            hash: name.clone(),
                        };
                        let data_path = file_entry.path();
                        let size = tokio::fs::metadata(&data_path).await.map(|m| m.len()).unwrap_or(0);
                        let meta_path = Self::meta_path_for(&data_path);
                        let mut meta = match tokio::fs::read(&meta_path).await {
                            Ok(json) => serde_json::from_slice::<BlobMeta>(&json).ok(),
                            Err(_) => None,
                        }
                        .filter(|m| m.size_bytes == size);
                        let mut dirty = meta.is_none();
                        if meta.is_none() {
                            // Reconstruct: keep the verified bytes, mint a record.
                            let mut fresh = BlobMeta::new(size, now, Hints::default());
                            fresh.seq = self.next_seq();
                            if kind == EntryKind::Cas
                                && let Ok(zst_meta) =
                                    tokio::fs::metadata(&self.cas_compressed_path(function, &name)).await
                            {
                                fresh.compressed_size_bytes = Some(zst_meta.len());
                            }
                            meta = Some(fresh);
                        }
                        let mut meta = meta.expect("reconstructed above");
                        if kind == EntryKind::Cas {
                            // Reconcile the compressed form with reality: an
                            // unclaimed .zst is garbage, a claimed-but-missing
                            // one must not count toward the quota.
                            let zst_path = self.cas_compressed_path(function, &name);
                            let zst_exists = tokio::fs::try_exists(&zst_path).await.unwrap_or(false);
                            if meta.compressed_size_bytes.is_none() && zst_exists {
                                let _ = tokio::fs::remove_file(&zst_path).await;
                            } else if meta.compressed_size_bytes.is_some() && !zst_exists {
                                meta.compressed_size_bytes = None;
                                dirty = true;
                            }
                        }
                        // Only repaired records rewrite their sidecar: a
                        // clean reopen must not fsync-rewrite every entry.
                        if dirty {
                            self.write_meta_file(&key, &meta).await?;
                        }
                        self.lock_index().insert(key, meta);
                    }
                }
            }
        }
        // Reseed the recency counter past every persisted seq so fresh
        // entries order after everything the scan recovered.
        let max_seq = self.lock_index().values().map(|m| m.seq).max().unwrap_or(0);
        self.seq.store(max_seq.saturating_add(1), Ordering::Relaxed);
        Ok(())
    }
}

/// True if two upload expectations describe the same blob (hashes compared
/// normalized: a resume may spell the hex differently).
fn same_blob(a: &UploadExpect, b: &UploadExpect) -> bool {
    a.size_bytes == b.size_bytes
        && a.proto_function == b.proto_function
        && a.compressed == b.compressed
        && normalize_hash(&a.hash) == normalize_hash(&b.hash)
}

/// fsync the directory containing `path` so a rename into it is durable.
/// Unix-only: opening a directory as a file is not portable, and without
/// it the dir entry may postdate a power cut.
async fn sync_parent_dir(path: &std::path::Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        tokio::fs::File::open(parent).await?.sync_all().await?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// If `hash` is the empty-blob digest of a supported function, return it.
/// The empty blob is synthesized as present everywhere without storage.
fn empty_blob_function(hash: &str) -> Option<DigestFunction> {
    // Computed per call (three tiny hashes); avoids hardcoding magic strings.
    for candidate in DigestFunction::by_hex_len(hash.len()) {
        if candidate.empty_hex() == hash {
            return Some(*candidate);
        }
    }
    None
}

/// Decode one zstd frame from `src` into `dst`, failing closed past
/// `expected_size` output bytes or on trailing input after the frame. Both
/// directions stream in 64 KiB chunks, so arbitrarily large blobs decode in
/// bounded memory. The decoder window is capped at `max_window`.
fn decode_frame<R: std::io::Read, W: std::io::Write>(
    src: R,
    mut dst: W,
    expected_size: u64,
    max_window: u64,
) -> Result<u64, StoreError> {
    use std::io::Read;
    // NOTE: FrameDecoder's Read impl only drains already-decoded bytes; it
    // does not drive decoding. StreamingDecoder drives + drains.
    let mut decoder = ruzstd::decoding::StreamingDecoder::new_with_max_window_size(src, max_window)
        .map_err(|e| StoreError::Decode(e.to_string()))?;
    let mut written = 0u64;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = decoder.read(&mut buf).map_err(|e| StoreError::Decode(e.to_string()))?;
        if n == 0 {
            break;
        }
        written += n as u64;
        if written > expected_size {
            return Err(StoreError::SizeMismatch {
                expected: expected_size,
                actual: written,
            });
        }
        dst.write_all(&buf[..n]).map_err(StoreError::Io)?;
    }
    // Trailing input after the frame is garbage: producers emit one frame
    // per blob.
    let mut src = decoder.into_inner();
    let mut probe = [0u8; 1];
    if src.read(&mut probe).map_err(StoreError::Io)? != 0 {
        return Err(StoreError::Decode("trailing data after zstd frame".to_string()));
    }
    Ok(written)
}

/// Blocking helper: decode the zstd file at `src` into `dst`, failing
/// closed unless the decoded size is exactly `expected_size`.
/// Runs on a worker thread; see [`Store::upload_finish`].
fn stream_decode_to_file(
    src: &std::path::Path,
    dst: &std::path::Path,
    expected_size: u64,
    max_window: u64,
) -> Result<u64, StoreError> {
    let src_file = std::fs::File::open(src).map_err(StoreError::Io)?;
    // Both spools are renamed into the store by the caller, so both are
    // synced here on their open handles.
    src_file.sync_all().map_err(StoreError::Io)?;
    let mut dst_file = std::fs::File::create(dst).map_err(StoreError::Io)?;
    let written = match decode_frame(src_file, &mut dst_file, expected_size, max_window) {
        Ok(written) => written,
        Err(e) => {
            drop(dst_file);
            let _ = std::fs::remove_file(dst);
            return Err(e);
        }
    };
    if written != expected_size {
        drop(dst_file);
        let _ = std::fs::remove_file(dst);
        return Err(StoreError::SizeMismatch {
            expected: expected_size,
            actual: written,
        });
    }
    dst_file.sync_all().map_err(StoreError::Io)?;
    Ok(written)
}

/// Decode zstd `data`, failing closed past `max_output` bytes (a
/// decompression bomb can never exceed the digest's own size, so callers
/// pass the expected size). The decoder window is capped at `max_window`.
fn decode_capped(data: &[u8], max_output: u64, max_window: u64) -> Result<Vec<u8>, StoreError> {
    let mut out = Vec::new();
    decode_frame(data, &mut out, max_output, max_window)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(name: &str) -> PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("reapid-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn hints() -> Hints {
        Hints {
            mnemonic: Some("Rustc".to_string()),
            target_id: Some("//foo:bar".to_string()),
            ..Hints::default()
        }
    }

    #[tokio::test]
    async fn cas_round_trip_with_hints() {
        let root = test_root("cas-round-trip");
        let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
        let data = b"hello world";
        let hash = DigestFunction::Sha256.hexdigest(data);
        let function = store
            .cas_put(data, &hash, data.len() as u64, 0, &hints(), 1000)
            .await
            .unwrap();
        assert_eq!(function, DigestFunction::Sha256);
        let (found, meta) = store.cas_lookup(&hash, 0).unwrap();
        assert_eq!(found, DigestFunction::Sha256);
        assert_eq!(meta.size_bytes, data.len() as u64);
        assert_eq!(meta.hints.mnemonic.as_deref(), Some("Rustc"));
        assert_eq!(meta.hints.target_id.as_deref(), Some("//foo:bar"));
        let bytes = tokio::fs::read(store.cas_data_path(found, &hash)).await.unwrap();
        assert_eq!(bytes, data);
        // Explicit function + uppercase hash also resolve.
        assert!(store.cas_lookup(&hash.to_uppercase(), 1).is_some());
        // Wrong size / wrong bytes / wrong function all fail.
        assert!(matches!(
            store.cas_put(data, &hash, 3, 0, &hints(), 1000).await,
            Err(StoreError::SizeMismatch { .. })
        ));
        assert!(matches!(
            store.cas_put(b"bye", &hash, 3, 0, &hints(), 1000).await,
            Err(StoreError::Resolve(ResolveError::Mismatch))
        ));
        assert!(matches!(
            store.cas_put(data, &hash, data.len() as u64, 2, &hints(), 1000).await,
            Err(StoreError::Resolve(_))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn empty_blob_is_synthesized() {
        let root = test_root("empty-synth");
        let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
        for function in [DigestFunction::Sha256, DigestFunction::Sha1, DigestFunction::Blake3] {
            let (found, meta) = store.cas_lookup(&function.empty_hex(), 0).unwrap();
            assert_eq!(found, function);
            assert_eq!(meta.size_bytes, 0);
        }
        assert!(store.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn compressed_round_trip() {
        let root = test_root("compressed");
        let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
        let data = vec![7u8; 100_000];
        let hash = DigestFunction::Sha256.hexdigest(&data);
        let zst =
            ruzstd::encoding::compress_to_vec(&data[..], ruzstd::encoding::CompressionLevel::Fastest);
        assert!(zst.len() < data.len());
        store
            .cas_put_compressed(&zst, &hash, data.len() as u64, 0, &hints(), 1000)
            .await
            .unwrap();
        let (function, meta) = store.cas_lookup(&hash, 0).unwrap();
        assert_eq!(meta.compressed_size_bytes, Some(zst.len() as u64));
        // Passthrough serves the sender's exact bytes.
        let zst_path = store.cas_compressed_path_for(function, &hash, 1000).await.unwrap().unwrap();
        assert_eq!(tokio::fs::read(&zst_path).await.unwrap(), zst);
        // Corrupt zstd fails closed.
        assert!(matches!(
            store
                .cas_put_compressed(b"not zstd", &hash, data.len() as u64, 0, &hints(), 1000)
                .await,
            Err(StoreError::Decode(_))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn encode_on_demand_for_plain_uploads() {
        let root = test_root("encode-on-demand");
        let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
        let data = vec![9u8; 50_000];
        let hash = DigestFunction::Sha256.hexdigest(&data);
        store
            .cas_put(&data, &hash, data.len() as u64, 0, &hints(), 1000)
            .await
            .unwrap();
        let (function, _) = store.cas_lookup(&hash, 0).unwrap();
        let zst_path = store.cas_compressed_path_for(function, &hash, 1000).await.unwrap().unwrap();
        let zst = tokio::fs::read(&zst_path).await.unwrap();
        // The on-demand encoding round-trips through the decoder.
        assert_eq!(decode_capped(&zst, data.len() as u64, 1 << 30).unwrap(), data);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn action_cache_round_trip() {
        let root = test_root("ac-round-trip");
        let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
        let action_hash = DigestFunction::Sha256.hexdigest(b"fake action");
        let result = b"fake action result";
        store.ac_put(&action_hash, 0, result, &hints(), 1000).await.unwrap();
        let (function, meta) = store.ac_lookup(&action_hash, 1).unwrap();
        assert_eq!(function, DigestFunction::Sha256);
        assert_eq!(meta.size_bytes, result.len() as u64);
        let bytes = tokio::fs::read(store.ac_data_path(function, &action_hash)).await.unwrap();
        assert_eq!(bytes, result);
        assert!(store.ac_lookup(&DigestFunction::Sha256.hexdigest(b"other"), 0).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn uploads_resume_and_commit() {
        let root = test_root("uploads");
        let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
        let data = b"chunked upload bytes";
        let hash = DigestFunction::Sha256.hexdigest(data);
        let expect = UploadExpect {
            hash: hash.clone(),
            size_bytes: data.len() as u64,
            proto_function: 0,
            compressed: false,
        };
        assert_eq!(store.upload_begin("u1", expect, &hints()).await.unwrap(), 0);
        assert_eq!(store.upload_append("u1", 0, &data[..7]).await.unwrap(), 7);
        // Wrong offset rejected; resume reports committed prefix.
        assert!(matches!(
            store.upload_append("u1", 0, &data[7..]).await,
            Err(StoreError::UploadOffset { .. })
        ));
        assert_eq!(store.upload_committed("u1").await, Some(7));
        assert_eq!(
            store.upload_append("u1", 7, &data[7..]).await.unwrap(),
            data.len() as u64
        );
        let (function, committed) = store.upload_finish("u1", 1000).await.unwrap();
        assert_eq!(function, DigestFunction::Sha256);
        assert_eq!(committed, data.len() as u64);
        assert!(store.cas_lookup(&hash, 0).is_some());
        assert_eq!(store.upload_committed("u1").await, None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn compressed_upload_reports_wire_size() {
        let root = test_root("upload-wire-size");
        let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
        let data = vec![5u8; 60_000];
        let hash = DigestFunction::Sha256.hexdigest(&data);
        let zst = ruzstd::encoding::compress_to_vec(
            &data[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        assert!(zst.len() < data.len());
        let expect = UploadExpect {
            hash: hash.clone(),
            size_bytes: data.len() as u64,
            proto_function: 0,
            compressed: true,
        };
        store.upload_begin("z1", expect, &hints()).await.unwrap();
        store.upload_append("z1", 0, &zst).await.unwrap();
        // finish() reports wire bytes (what WriteResponse must echo), while
        // the canonical decoded bytes land in CAS.
        let (function, committed) = store.upload_finish("z1", 1000).await.unwrap();
        assert_eq!(function, DigestFunction::Sha256);
        assert_eq!(committed, zst.len() as u64);
        let (_, meta) = store.cas_lookup(&hash, 0).unwrap();
        assert_eq!(meta.size_bytes, data.len() as u64);
        assert_eq!(meta.compressed_size_bytes, Some(zst.len() as u64));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn concurrent_appends_to_same_upload_serialize() {
        let root = test_root("upload-race");
        let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
        let data = b"racing writers";
        let hash = DigestFunction::Sha256.hexdigest(data);
        let expect = UploadExpect {
            hash: hash.clone(),
            size_bytes: data.len() as u64,
            proto_function: 0,
            compressed: false,
        };
        store.upload_begin("race", expect, &hints()).await.unwrap();
        // Two writers append the same full bytes at offset 0 concurrently:
        // check + write + accounting hold one lock, so exactly one wins and
        // the spool holds exactly one copy (never interleaved bytes).
        let (a, b) = tokio::join!(
            store.upload_append("race", 0, data),
            store.upload_append("race", 0, data)
        );
        assert_ne!(a.is_ok(), b.is_ok(), "{a:?} {b:?}");
        assert_eq!(store.upload_committed("race").await, Some(data.len() as u64));
        let (function, committed) = store.upload_finish("race", 1000).await.unwrap();
        assert_eq!(function, DigestFunction::Sha256);
        assert_eq!(committed, data.len() as u64);
        assert_eq!(
            tokio::fs::read(store.cas_data_path(function, &hash)).await.unwrap(),
            data
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn resume_with_different_blob_rejected() {
        let root = test_root("upload-mismatch");
        let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
        let data = b"original bytes";
        let hash = DigestFunction::Sha256.hexdigest(data);
        store
            .upload_begin(
                "u1",
                UploadExpect {
                    hash: hash.clone(),
                    size_bytes: data.len() as u64,
                    proto_function: 0,
                    compressed: false,
                },
                &hints(),
            )
            .await
            .unwrap();
        store.upload_append("u1", 0, &data[..4]).await.unwrap();
        // Same id, different blob: rejected; the original prefix is intact.
        let other = DigestFunction::Sha256.hexdigest(b"other bytes!");
        assert!(matches!(
            store
                .upload_begin(
                    "u1",
                    UploadExpect {
                        hash: other,
                        size_bytes: data.len() as u64,
                        proto_function: 0,
                        compressed: false,
                    },
                    &hints(),
                )
                .await,
            Err(StoreError::UploadMismatch)
        ));
        assert_eq!(store.upload_committed("u1").await, Some(4));
        // Same id, same blob spelled in uppercase: resumes fine.
        assert_eq!(
            store
                .upload_begin(
                    "u1",
                    UploadExpect {
                        hash: hash.to_uppercase(),
                        size_bytes: data.len() as u64,
                        proto_function: 0,
                        compressed: false,
                    },
                    &hints(),
                )
                .await
                .unwrap(),
            4
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn scan_repairs_and_rebuilds() {
        let root = test_root("scan");
        let data = b"persistent bytes";
        let hash = DigestFunction::Sha256.hexdigest(data);
        {
            let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
            store.cas_put(data, &hash, data.len() as u64, 0, &hints(), 1000).await.unwrap();
            // Corrupt the sidecar; the bytes stay valid.
            let meta = Store::meta_path_for(&store.cas_data_path(DigestFunction::Sha256, &hash));
            tokio::fs::write(&meta, b"{oops").await.unwrap();
            // Orphan sidecar + orphan .zst get swept.
            let orphan =
                store.cas_data_path(DigestFunction::Sha256, &DigestFunction::Sha256.hexdigest(b"x"));
            let _ = tokio::fs::create_dir_all(orphan.parent().unwrap()).await;
            tokio::fs::write(Store::meta_path_for(&orphan), b"{}").await.unwrap();
        }
        let store = Store::open(root.clone(), 1 << 30, 2000).await.unwrap();
        assert_eq!(store.len(), 1);
        let (_, meta) = store.cas_lookup(&hash, 0).unwrap();
        assert_eq!(meta.size_bytes, data.len() as u64);
        assert_eq!(meta.first_seen, 2000);
        assert_eq!(store.total_bytes(), data.len() as u64);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn reopen_preserves_everything() {
        let root = test_root("reopen-clean");
        let plain = b"plain blob";
        let plain_hash = DigestFunction::Sha256.hexdigest(plain);
        let sha1_data = b"sha1 blob";
        let sha1_hash = DigestFunction::Sha1.hexdigest(sha1_data);
        let blake3_data = b"blake3 blob";
        let blake3_hash = DigestFunction::Blake3.hexdigest(blake3_data);
        let zst_data = vec![4u8; 40_000];
        let zst_hash = DigestFunction::Sha256.hexdigest(&zst_data);
        let zst = ruzstd::encoding::compress_to_vec(
            &zst_data[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        let action_hash = DigestFunction::Sha256.hexdigest(b"action");
        let result = b"action result";
        let (len, total) = {
            let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
            store
                .cas_put(plain, &plain_hash, plain.len() as u64, 0, &hints(), 1000)
                .await
                .unwrap();
            store
                .cas_put(
                    sha1_data,
                    &sha1_hash,
                    sha1_data.len() as u64,
                    DigestFunction::Sha1.to_proto(),
                    &hints(),
                    1000,
                )
                .await
                .unwrap();
            store
                .cas_put(
                    blake3_data,
                    &blake3_hash,
                    blake3_data.len() as u64,
                    DigestFunction::Blake3.to_proto(),
                    &hints(),
                    1000,
                )
                .await
                .unwrap();
            store
                .cas_put_compressed(&zst, &zst_hash, zst_data.len() as u64, 0, &hints(), 1000)
                .await
                .unwrap();
            store.ac_put(&action_hash, 0, result, &hints(), 1000).await.unwrap();
            // Persisted access stats survive; the untouched blob keeps zeros.
            store
                .record_access(
                    &EntryKey {
                        kind: EntryKind::Cas,
                        function: DigestFunction::Sha256,
                        hash: plain_hash.clone(),
                    },
                    1500,
                )
                .await
                .unwrap();
            // A clean eviction stays evicted across the restart.
            let doomed = b"doomed";
            let doomed_hash = DigestFunction::Sha256.hexdigest(doomed);
            store
                .cas_put(doomed, &doomed_hash, doomed.len() as u64, 0, &hints(), 1000)
                .await
                .unwrap();
            store
                .remove(&EntryKey {
                    kind: EntryKind::Cas,
                    function: DigestFunction::Sha256,
                    hash: doomed_hash.clone(),
                })
                .await;
            assert!(store.cas_lookup(&doomed_hash, 0).is_none());
            (store.len(), store.total_bytes())
        };
        let store = Store::open(root.clone(), 1 << 30, 2000).await.unwrap();
        assert_eq!(store.len(), len);
        assert_eq!(store.total_bytes(), total);
        // Every blob readable with identical bytes and metadata (times and
        // hints preserved, not reset to the reopen moment).
        for (hash, function, want) in [
            (plain_hash.clone(), DigestFunction::Sha256, plain.to_vec()),
            (sha1_hash.clone(), DigestFunction::Sha1, sha1_data.to_vec()),
            (blake3_hash.clone(), DigestFunction::Blake3, blake3_data.to_vec()),
            (zst_hash.clone(), DigestFunction::Sha256, zst_data.clone()),
        ] {
            let (found, meta) = store.cas_lookup(&hash, 0).unwrap();
            assert_eq!(found, function);
            assert_eq!(meta.size_bytes, want.len() as u64);
            assert_eq!(meta.first_seen, 1000);
            assert_eq!(meta.hints.mnemonic.as_deref(), Some("Rustc"));
            assert_eq!(meta.hints.target_id.as_deref(), Some("//foo:bar"));
            assert_eq!(tokio::fs::read(store.cas_data_path(found, &hash)).await.unwrap(), want);
        }
        let (_, plain_meta) = store.cas_lookup(&plain_hash, 0).unwrap();
        assert_eq!(plain_meta.access_count, 1);
        assert_eq!(plain_meta.last_access, 1500);
        let (_, zst_meta) = store.cas_lookup(&zst_hash, 0).unwrap();
        assert_eq!(zst_meta.compressed_size_bytes, Some(zst.len() as u64));
        // The ActionCache entry round-trips too.
        let (function, _) = store.ac_lookup(&action_hash, 0).unwrap();
        assert_eq!(
            tokio::fs::read(store.ac_data_path(function, &action_hash)).await.unwrap(),
            result
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn seq_reseeds_past_max() {
        let root = test_root("seq-reseed");
        let hash_a = DigestFunction::Sha256.hexdigest(b"a");
        let hash_b = DigestFunction::Sha256.hexdigest(b"b");
        let hash_c = DigestFunction::Sha256.hexdigest(b"c");
        {
            let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
            store.cas_put(b"a", &hash_a, 1, 0, &Hints::default(), 1000).await.unwrap();
            store.cas_put(b"b", &hash_b, 1, 0, &Hints::default(), 1000).await.unwrap();
            assert!(store.cas_lookup(&hash_a, 0).unwrap().1.seq < store.cas_lookup(&hash_b, 0).unwrap().1.seq);
        }
        let store = Store::open(root.clone(), 1 << 30, 2000).await.unwrap();
        store.cas_put(b"c", &hash_c, 1, 0, &Hints::default(), 2000).await.unwrap();
        // LRU order spans the restart: a < b < c.
        let seq_a = store.cas_lookup(&hash_a, 0).unwrap().1.seq;
        let seq_b = store.cas_lookup(&hash_b, 0).unwrap().1.seq;
        let seq_c = store.cas_lookup(&hash_c, 0).unwrap().1.seq;
        assert!(seq_a < seq_b && seq_b < seq_c, "{seq_a} {seq_b} {seq_c}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn stale_tmp_files_ignored_and_swept() {
        let root = test_root("tmp-sweep");
        let data = b"real blob";
        let hash = DigestFunction::Sha256.hexdigest(data);
        {
            let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
            store.cas_put(data, &hash, data.len() as u64, 0, &hints(), 1000).await.unwrap();
            // Crash debris: temp files a crash interrupted mid-rename, for
            // both data and sidecar shapes.
            let data_tmp = store
                .cas_data_path(DigestFunction::Sha256, &hash)
                .with_extension("tmp-4242-7");
            tokio::fs::write(&data_tmp, b"partial").await.unwrap();
            let meta_path =
                Store::meta_path_for(&store.cas_data_path(DigestFunction::Sha256, &hash));
            let meta_tmp = meta_path.with_extension("tmp-4242-8");
            tokio::fs::write(&meta_tmp, b"partial").await.unwrap();
        }
        let store = Store::open(root.clone(), 1 << 30, 2000).await.unwrap();
        assert!(store.cas_lookup(&hash, 0).is_some());
        let shard = store.cas_data_path(DigestFunction::Sha256, &hash).parent().unwrap().to_path_buf();
        let mut entries = tokio::fs::read_dir(&shard).await.unwrap();
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
        assert!(!names.iter().any(|n| n.contains(".tmp-")), "{names:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn scan_leaves_clean_sidecars_untouched() {
        let root = test_root("scan-clean");
        let data = b"clean blob";
        let hash = DigestFunction::Sha256.hexdigest(data);
        let meta_path = {
            let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
            store.cas_put(data, &hash, data.len() as u64, 0, &hints(), 1000).await.unwrap();
            Store::meta_path_for(&store.cas_data_path(DigestFunction::Sha256, &hash))
        };
        // Backdate the sidecar: a clean reopen must index it without
        // rewriting (no fsync storm on large stores).
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&meta_path)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let store = Store::open(root.clone(), 1 << 30, 2000).await.unwrap();
        assert!(store.cas_lookup(&hash, 0).is_some());
        assert_eq!(std::fs::metadata(&meta_path).unwrap().modified().unwrap(), old);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn data_without_sidecar_is_kept() {
        // Crash between the data rename and the sidecar write: the bytes
        // were verified before the rename, so the scan keeps them and mints
        // a record instead of dropping the entry.
        let root = test_root("bare-data");
        let data = b"verified before rename";
        let hash = DigestFunction::Sha256.hexdigest(data);
        {
            let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
            let path = store.cas_data_path(DigestFunction::Sha256, &hash);
            tokio::fs::create_dir_all(path.parent().unwrap()).await.unwrap();
            tokio::fs::write(&path, data).await.unwrap();
        }
        let store = Store::open(root.clone(), 1 << 30, 2000).await.unwrap();
        let (_, meta) = store.cas_lookup(&hash, 0).unwrap();
        assert_eq!(meta.size_bytes, data.len() as u64);
        assert!(meta.hints.is_empty());
        assert_eq!(meta.first_seen, 2000);
        // The repaired record persists: a second reopen keeps it verbatim.
        drop(store);
        let store = Store::open(root.clone(), 1 << 30, 3000).await.unwrap();
        let (_, meta) = store.cas_lookup(&hash, 0).unwrap();
        assert_eq!(meta.first_seen, 2000);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn unclaimed_zst_dropped_canonical_kept() {
        // Crash after the .zst rename but before the sidecar claimed it:
        // the canonical bytes are intact, so the scan drops only the
        // unclaimed compressed form (re-encodable on demand).
        let root = test_root("stray-zst");
        let data = vec![6u8; 30_000];
        let hash = DigestFunction::Sha256.hexdigest(&data);
        {
            let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
            store.cas_put(&data, &hash, data.len() as u64, 0, &hints(), 1000).await.unwrap();
            let zst_path = store.cas_compressed_path(DigestFunction::Sha256, &hash);
            tokio::fs::write(&zst_path, b"unclaimed").await.unwrap();
        }
        let store = Store::open(root.clone(), 1 << 30, 2000).await.unwrap();
        let (function, meta) = store.cas_lookup(&hash, 0).unwrap();
        assert_eq!(tokio::fs::read(store.cas_data_path(function, &hash)).await.unwrap(), data);
        assert_eq!(meta.compressed_size_bytes, None);
        assert!(!tokio::fs::try_exists(store.cas_compressed_path(function, &hash)).await.unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn partial_uploads_wiped_on_open() {
        let root = test_root("upload-wipe");
        let data = b"never finished";
        let hash = DigestFunction::Sha256.hexdigest(data);
        {
            let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
            store
                .upload_begin(
                    "u1",
                    UploadExpect {
                        hash: hash.clone(),
                        size_bytes: data.len() as u64,
                        proto_function: 0,
                        compressed: false,
                    },
                    &hints(),
                )
                .await
                .unwrap();
            store.upload_append("u1", 0, &data[..5]).await.unwrap();
            assert_eq!(store.upload_committed("u1").await, Some(5));
        }
        let store = Store::open(root.clone(), 1 << 30, 2000).await.unwrap();
        assert_eq!(store.upload_committed("u1").await, None);
        assert!(store.cas_lookup(&hash, 0).is_none());
        let mut entries = tokio::fs::read_dir(root.join("uploads")).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(&root);
    }
}
