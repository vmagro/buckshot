//! gRPC services: CAS, ActionCache, Capabilities, ByteStream.
//!
//! One [`Api`] struct implements all four traits over shared state. Every
//! mutating RPC runs an eviction pass before returning. The Execution API is
//! deliberately absent: this server never executes, and buck2 is configured
//! with `remote_enabled = False`.

use std::collections::HashSet;
use std::sync::Arc;

use prost::Message;

use crate::config::Config;
use crate::digest::normalize_hash;
use crate::mux::Mux;
use crate::policy;
use crate::store::EntryKey;
use crate::store::EntryKind;
use crate::store::Hints;
use crate::store::Store;
use crate::store::StoreError;
use crate::store::UploadExpect;

/// Shared daemon state behind every service.
pub struct Shared {
    /// The local store. Present exactly when some backend is `self`
    /// (enforced by config validation); pure proxies have none.
    pub store: Option<Store>,
    pub config: Config,
    pub mux: Mux,
    /// Serializes eviction passes so concurrent writers cannot over-evict.
    evict_lock: tokio::sync::Mutex<()>,
}

impl Shared {
    pub fn new(store: Option<Store>, config: Config, mux: Mux) -> Shared {
        Shared {
            store,
            config,
            mux,
            evict_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The local store. Only called on the `self` path, which routing
    /// reaches only when a `self` backend exists, which validation allows
    /// only with a store: the expect documents that chain.
    pub fn local_store(&self) -> &Store {
        self.store
            .as_ref()
            .expect("local path without a store: self backend requires store_dir")
    }

    /// Route one RPC through the mux. Returns the forwarding target, or
    /// `None` when the caller should serve from the local store.
    pub fn mux_route(&self, hints: &Hints) -> Option<(Mux, usize)> {
        match self.mux.route(hints.tool_invocation_id.as_deref()) {
            crate::mux::Route::Local => None,
            crate::mux::Route::Remote(index) => Some((self.mux.clone(), index)),
        }
    }

    /// Run one eviction pass. Called after every locally-served mutating
    /// RPC and at startup; a no-op without a store (forwarded writes are
    /// evicted by the backend that holds them).
    pub async fn evict_pass(&self) {
        let Some(store) = &self.store else {
            return;
        };
        let _guard = self.evict_lock.lock().await;
        let deleted = policy::evict(store, &self.config.rules, self.config.max_store_bytes, now_unix()).await;
        if deleted > 0 {
            tracing::debug!(deleted, total_bytes = store.total_bytes(), "evicted entries");
        }
    }
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn store_status(e: StoreError) -> tonic::Status {
    match e {
        StoreError::Io(e) => tonic::Status::internal(format!("store i/o: {e}")),
        StoreError::Resolve(e) => tonic::Status::invalid_argument(e.to_string()),
        StoreError::SizeMismatch { .. } => tonic::Status::invalid_argument(e.to_string()),
        // Configured limits fail fast rather than inviting retry storms.
        StoreError::TooLarge { .. } => tonic::Status::invalid_argument(e.to_string()),
        StoreError::Decode(_) => tonic::Status::invalid_argument(e.to_string()),
        StoreError::UploadNotFound => tonic::Status::not_found(e.to_string()),
        StoreError::UploadOffset { .. } | StoreError::UploadMismatch => {
            tonic::Status::invalid_argument(e.to_string())
        }
        StoreError::UploadFinished => tonic::Status::failed_precondition(e.to_string()),
    }
}

fn response_status(code: tonic::Code, message: String) -> rpc_status::Status {
    rpc_status::Status {
        code: code as i32,
        message,
        details: Vec::new(),
    }
}

/// One-line summary of captured hints for debug logs.
fn hints_summary(hints: &Hints) -> String {
    let mut parts = Vec::new();
    if let Some(m) = &hints.mnemonic {
        parts.push(format!("mnemonic={m}"));
    }
    if let Some(t) = &hints.target_id {
        parts.push(format!("target={t}"));
    }
    if let Some(i) = &hints.tool_invocation_id {
        parts.push(format!("invocation={i}"));
    }
    if let Some(c) = &hints.configuration_id {
        parts.push(format!("config={c}"));
    }
    if parts.is_empty() {
        return "(no hints)".to_string();
    }
    parts.join(" ")
}

/// `build.bazel.remote.execution.v2.requestmetadata-bin` header: the client's
/// self-description (tool, action, target, invocation). Absent or corrupt on
/// any call degrades to empty hints, never an error.
fn request_hints<T>(request: &tonic::Request<T>) -> Hints {
    const HEADER: &str = "build.bazel.remote.execution.v2.requestmetadata-bin";
    let metadata = request.metadata().get_bin(HEADER);
    let Some(value) = metadata else {
        return Hints::default();
    };
    // NOTE: as_ref()/as_encoded_bytes() yield base64 for Binary values;
    // to_bytes() base64-decodes to the raw protobuf.
    let bytes = match value.to_bytes() {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::debug!(error = %e, "ignoring corrupt requestmetadata-bin");
            return Hints::default();
        }
    };
    let meta = match reapi::RequestMetadata::decode(&bytes[..]) {
        Ok(meta) => meta,
        Err(e) => {
            tracing::debug!(error = %e, "ignoring corrupt requestmetadata-bin");
            return Hints::default();
        }
    };
    fn some(s: String) -> Option<String> {
        match s.is_empty() {
            true => None,
            false => Some(s),
        }
    }
    Hints {
        action_id: some(meta.action_id),
        tool_invocation_id: some(meta.tool_invocation_id),
        correlated_invocations_id: some(meta.correlated_invocations_id),
        mnemonic: some(meta.action_mnemonic),
        target_id: some(meta.target_id),
        configuration_id: some(meta.configuration_id),
    }
}

// ---- ByteStream resource names ----

/// Parsed ByteStream resource name. The instance prefix (any leading
/// segments) is accepted and ignored: this server has one global namespace.
#[derive(Debug, PartialEq, Eq)]
pub struct ResourceName {
    /// Set for `uploads/<uuid>/...` (resumable); `None` for direct `blobs/`
    /// writes, which the server funnels through a synthetic upload id.
    pub upload_id: Option<String>,
    pub hash: String,
    pub size_bytes: u64,
    pub compressed: bool,
}

pub fn parse_resource_name(name: &str) -> Result<ResourceName, tonic::Status> {
    let invalid = |detail: &str| tonic::Status::invalid_argument(format!("bad resource name {name:?}: {detail}"));
    let segments: Vec<&str> = name.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 3 {
        return Err(invalid("want [.../]blobs/<hash>/<size>"));
    }
    let (hash, size_raw) = (segments[segments.len() - 2], segments[segments.len() - 1]);
    let Some(hash) = normalize_hash(hash) else {
        return Err(invalid("hash is not hex"));
    };
    let Ok(size_bytes) = size_raw.parse::<u64>() else {
        return Err(invalid("size is not a u64"));
    };
    let mut rest = &segments[..segments.len() - 2];
    let mut upload_id = None;
    // Innermost marker: `blobs` or `compressed-blobs/<compressor>`.
    let compressed = match rest.last() {
        Some(&"blobs") => {
            rest = &rest[..rest.len() - 1];
            false
        }
        _ if rest.len() >= 2 && rest[rest.len() - 2] == "compressed-blobs" => {
            let compressor = rest[rest.len() - 1];
            if compressor != "zstd" {
                return Err(invalid(&format!("unsupported compressor {compressor:?}")));
            }
            rest = &rest[..rest.len() - 2];
            true
        }
        _ => return Err(invalid("want [.../]blobs/<hash>/<size>")),
    };
    if rest.len() >= 2 && rest[rest.len() - 2] == "uploads" {
        upload_id = Some(rest[rest.len() - 1].to_string());
    } else if rest.last() == Some(&"uploads") {
        return Err(invalid("uploads without an id"));
    }
    // Anything left is the instance name; ignored.
    Ok(ResourceName {
        upload_id,
        hash,
        size_bytes,
        compressed,
    })
}

// ---- the API ----

/// zstd encoding of the empty blob, for compressed reads of it (which has no
/// stored form to serve passthrough).
fn zstd_empty() -> Vec<u8> {
    let empty: &[u8] = &[];
    ruzstd::encoding::compress_to_vec(empty, ruzstd::encoding::CompressionLevel::Fastest)
}

#[derive(Clone)]
pub struct Api {
    shared: Arc<Shared>,
}

impl Api {
    pub fn new(shared: Arc<Shared>) -> Api {
        Api { shared }
    }
}

/// Map a store failure to a per-blob response status. Every variant is a
/// client error except I/O; Upload* variants are unreachable on batch paths.
fn per_blob_error(e: &StoreError) -> rpc_status::Status {
    let code = match e {
        StoreError::Io(_) => tonic::Code::Internal,
        StoreError::Resolve(_)
        | StoreError::SizeMismatch { .. }
        | StoreError::TooLarge { .. }
        | StoreError::Decode(_) => tonic::Code::InvalidArgument,
        StoreError::UploadNotFound
        | StoreError::UploadOffset { .. }
        | StoreError::UploadFinished
        | StoreError::UploadMismatch => tonic::Code::Internal,
    };
    response_status(code, e.to_string())
}

#[tonic::async_trait]
impl reapi::content_addressable_storage_server::ContentAddressableStorage for Api {
    async fn find_missing_blobs(
        &self,
        request: tonic::Request<reapi::FindMissingBlobsRequest>,
    ) -> Result<tonic::Response<reapi::FindMissingBlobsResponse>, tonic::Status> {
        let hints = request_hints(&request);
        if let Some((mux, backend)) = self.shared.mux_route(&hints) {
            return mux.forward_find_missing_blobs(backend, request).await;
        }
        let req = request.into_inner();
        tracing::debug!(
            digests = req.blob_digests.len(),
            digest_function = req.digest_function,
            hints = %hints_summary(&hints),
            "find_missing_blobs"
        );
        let mut missing = Vec::new();
        // Gentle per-digest semantics: malformed digests report missing (the
        // follow-up upload fails loudly), so one bad digest cannot fail a
        // thousand-digest batch.
        for digest in &req.blob_digests {
            let hit = if digest.size_bytes < 0 {
                None
            } else {
                self.shared
                    .local_store()
                    .cas_lookup(&digest.hash, req.digest_function)
                    .filter(|(_, meta)| meta.size_bytes == digest.size_bytes as u64)
            };
            match hit {
                Some((function, _)) => {
                    if let Some(hash) = normalize_hash(&digest.hash) {
                        self.shared.local_store().touch(
                            &EntryKey {
                                kind: EntryKind::Cas,
                                function,
                                hash,
                            },
                            now_unix(),
                        );
                    }
                }
                None => missing.push(digest.clone()),
            }
        }
        tracing::debug!(missing = missing.len(), "find_missing_blobs done");
        Ok(tonic::Response::new(reapi::FindMissingBlobsResponse {
            missing_blob_digests: missing,
        }))
    }

    async fn batch_update_blobs(
        &self,
        request: tonic::Request<reapi::BatchUpdateBlobsRequest>,
    ) -> Result<tonic::Response<reapi::BatchUpdateBlobsResponse>, tonic::Status> {
        let hints = request_hints(&request);
        if let Some((mux, backend)) = self.shared.mux_route(&hints) {
            return mux.forward_batch_update_blobs(backend, request).await;
        }
        let req = request.into_inner();
        tracing::debug!(
            requests = req.requests.len(),
            digest_function = req.digest_function,
            hints = %hints_summary(&hints),
            "batch_update_blobs"
        );
        let total: u64 = req.requests.iter().map(|r| r.data.len() as u64).sum();
        if total > self.shared.config.max_batch_bytes {
            return Err(tonic::Status::invalid_argument(format!(
                "batch of {total} bytes exceeds max_batch_bytes {}",
                self.shared.config.max_batch_bytes
            )));
        }
        let now = now_unix();
        let mut responses = Vec::with_capacity(req.requests.len());
        for update in &req.requests {
            let Some(digest) = &update.digest else {
                return Err(tonic::Status::invalid_argument("batch update without a digest"));
            };
            if digest.size_bytes < 0 {
                return Err(tonic::Status::invalid_argument(format!(
                    "negative size for {:?}",
                    digest.hash
                )));
            }
            // Per-blob failures ride in the response; the RPC itself fails
            // only for malformed requests (missing digest, negative size,
            // over-cap batch).
            let store = self.shared.local_store();
            let status = match update.compressor {
                0 => {
                    store
                        .cas_put(
                            &update.data,
                            &digest.hash,
                            digest.size_bytes as u64,
                            req.digest_function,
                            &hints,
                            now,
                        )
                        .await
                        .err()
                        .map(|e| per_blob_error(&e))
                }
                1 => {
                    store
                        .cas_put_compressed(
                            &update.data,
                            &digest.hash,
                            digest.size_bytes as u64,
                            req.digest_function,
                            &hints,
                            now,
                        )
                        .await
                        .err()
                        .map(|e| per_blob_error(&e))
                }
                c => Some(response_status(
                    tonic::Code::InvalidArgument,
                    format!("unsupported compressor {c}"),
                )),
            };
            responses.push(reapi::batch_update_blobs_response::Response {
                digest: Some(digest.clone()),
                status,
            });
        }
        let failed = responses.iter().filter(|r| r.status.is_some()).count();
        tracing::debug!(stored = responses.len() - failed, failed, "batch_update_blobs done");
        self.shared.evict_pass().await;
        Ok(tonic::Response::new(reapi::BatchUpdateBlobsResponse { responses }))
    }

    async fn batch_read_blobs(
        &self,
        request: tonic::Request<reapi::BatchReadBlobsRequest>,
    ) -> Result<tonic::Response<reapi::BatchReadBlobsResponse>, tonic::Status> {
        let hints = request_hints(&request);
        if let Some((mux, backend)) = self.shared.mux_route(&hints) {
            return mux.forward_batch_read_blobs(backend, request).await;
        }
        let req = request.into_inner();
        tracing::debug!(
            digests = req.digests.len(),
            hints = %hints_summary(&hints),
            "batch_read_blobs"
        );
        let now = now_unix();
        let mut responses = Vec::with_capacity(req.digests.len());
        for digest in &req.digests {
            let response = self.read_one_blob(digest, &req.acceptable_compressors, req.digest_function, now).await;
            responses.push(response);
        }
        Ok(tonic::Response::new(reapi::BatchReadBlobsResponse { responses }))
    }

    type GetTreeStream = tokio_stream::wrappers::ReceiverStream<Result<reapi::GetTreeResponse, tonic::Status>>;

    async fn get_tree(
        &self,
        request: tonic::Request<reapi::GetTreeRequest>,
    ) -> Result<tonic::Response<Self::GetTreeStream>, tonic::Status> {
        let hints = request_hints(&request);
        if let Some((mux, backend)) = self.shared.mux_route(&hints) {
            return mux.forward_get_tree(backend, request).await;
        }
        let req = request.into_inner();
        tracing::debug!(hints = %hints_summary(&hints), "get_tree");
        let Some(root) = &req.root_digest else {
            return Err(tonic::Status::invalid_argument("get_tree without a root digest"));
        };
        if root.size_bytes < 0 || normalize_hash(&root.hash).is_none() {
            return Err(tonic::Status::invalid_argument("malformed get_tree root digest"));
        }
        // No paging: one response carries the whole tree. Trees that exceed
        // the cap fail rather than silently truncating (a truncated tree
        // would corrupt every consumer).
        const MAX_DIRECTORIES: usize = 100_000;
        let mut directories = Vec::new();
        let mut queue = vec![root.clone()];
        let mut seen = HashSet::new();
        while let Some(digest) = queue.pop() {
            if !seen.insert((digest.hash.clone(), digest.size_bytes)) {
                continue;
            }
            let Some(bytes) = self.tree_node(&digest, req.digest_function).await? else {
                return Err(tonic::Status::not_found(format!("tree blob {:?} missing", digest.hash)));
            };
            let directory = reapi::Directory::decode(&bytes[..])
                .map_err(|e| tonic::Status::internal(format!("corrupt directory blob: {e}")))?;
            for child in &directory.directories {
                if let Some(child_digest) = &child.digest {
                    queue.push(child_digest.clone());
                }
            }
            directories.push(directory);
            if directories.len() > MAX_DIRECTORIES {
                return Err(tonic::Status::resource_exhausted(format!(
                    "tree exceeds {MAX_DIRECTORIES} directories"
                )));
            }
        }
        tracing::debug!(directories = directories.len(), "get_tree done");
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let _ = tx
            .send(Ok(reapi::GetTreeResponse {
                directories,
                next_page_token: String::new(),
            }))
            .await;
        Ok(tonic::Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    async fn split_blob(
        &self,
        request: tonic::Request<reapi::SplitBlobRequest>,
    ) -> Result<tonic::Response<reapi::SplitBlobResponse>, tonic::Status> {
        let hints = request_hints(&request);
        if let Some((mux, backend)) = self.shared.mux_route(&hints) {
            return mux.forward_split_blob(backend, request).await;
        }
        Err(tonic::Status::unimplemented("chunking APIs are not supported; use whole blobs"))
    }

    async fn splice_blob(
        &self,
        request: tonic::Request<reapi::SpliceBlobRequest>,
    ) -> Result<tonic::Response<reapi::SpliceBlobResponse>, tonic::Status> {
        let hints = request_hints(&request);
        if let Some((mux, backend)) = self.shared.mux_route(&hints) {
            return mux.forward_splice_blob(backend, request).await;
        }
        Err(tonic::Status::unimplemented("chunking APIs are not supported; use whole blobs"))
    }

    type GetChunkMappingStream =
        tokio_stream::wrappers::ReceiverStream<Result<reapi::GetChunkMappingResponse, tonic::Status>>;

    async fn get_chunk_mapping(
        &self,
        request: tonic::Request<reapi::GetChunkMappingRequest>,
    ) -> Result<tonic::Response<Self::GetChunkMappingStream>, tonic::Status> {
        let hints = request_hints(&request);
        if let Some((mux, backend)) = self.shared.mux_route(&hints) {
            return mux.forward_get_chunk_mapping(backend, request).await;
        }
        Err(tonic::Status::unimplemented("chunking APIs are not supported; use whole blobs"))
    }

    async fn register_chunk_mapping(
        &self,
        request: tonic::Request<tonic::Streaming<reapi::RegisterChunkMappingRequest>>,
    ) -> Result<tonic::Response<reapi::RegisterChunkMappingResponse>, tonic::Status> {
        let hints = request_hints(&request);
        if let Some((mux, backend)) = self.shared.mux_route(&hints) {
            return mux.forward_register_chunk_mapping(backend, request).await;
        }
        Err(tonic::Status::unimplemented("chunking APIs are not supported; use whole blobs"))
    }
}

impl Api {
    async fn read_one_blob(
        &self,
        digest: &reapi::Digest,
        acceptable_compressors: &[i32],
        digest_function: i32,
        now: u64,
    ) -> reapi::batch_read_blobs_response::Response {
        let failure = |code: tonic::Code, message: String| reapi::batch_read_blobs_response::Response {
            digest: Some(digest.clone()),
            data: Vec::new(),
            compressor: 0,
            status: Some(response_status(code, message)),
        };
        if digest.size_bytes < 0 {
            return failure(tonic::Code::InvalidArgument, format!("negative size for {:?}", digest.hash));
        }
        let Some(hash) = normalize_hash(&digest.hash) else {
            return failure(tonic::Code::InvalidArgument, format!("malformed hash {:?}", digest.hash));
        };
        let Some((function, meta)) = self.shared.local_store().cas_lookup(&hash, digest_function)
        else {
            return failure(tonic::Code::NotFound, format!("blob {hash:?} missing"));
        };
        if meta.size_bytes != digest.size_bytes as u64 {
            return failure(tonic::Code::NotFound, format!("blob {hash:?} missing"));
        }
        // Serve identity unless the client rules it out, then zstd if
        // offered; anything else is a client error, not a miss.
        let want_zstd = !acceptable_compressors.is_empty() && !acceptable_compressors.contains(&0);
        if want_zstd && !acceptable_compressors.contains(&1) {
            return failure(
                tonic::Code::InvalidArgument,
                format!("unsupported acceptable_compressors {acceptable_compressors:?}"),
            );
        }
        if meta.size_bytes > self.shared.config.max_batch_bytes {
            return failure(
                tonic::Code::FailedPrecondition,
                format!(
                    "blob {:?} ({} bytes) exceeds max_batch_bytes {}; use ByteStream",
                    digest.hash, meta.size_bytes, self.shared.config.max_batch_bytes
                ),
            );
        }
        // The empty blob has no stored form; synthesize it (in the negotiated
        // form, so a zstd-only client gets bytes it can decode).
        if meta.size_bytes == 0 {
            let (data, compressor) = match want_zstd {
                true => (zstd_empty(), 1),
                false => (Vec::new(), 0),
            };
            return reapi::batch_read_blobs_response::Response {
                digest: Some(digest.clone()),
                data,
                compressor,
                status: None,
            };
        }
        let key = EntryKey {
            kind: EntryKind::Cas,
            function,
            hash: hash.clone(),
        };
        // Only I/O can fail here (the blob was looked up above).
        let (path, compressor) = match want_zstd {
            true => {
                match self.shared.local_store().cas_compressed_path_for(function, &hash, now).await {
                    Ok(Some(path)) => (path, 1),
                    Ok(None) => return failure(tonic::Code::NotFound, format!("blob {hash:?} missing")),
                    Err(StoreError::Io(e)) => {
                        return failure(tonic::Code::Internal, format!("store i/o: {e}"));
                    }
                    Err(e) => return failure(tonic::Code::Internal, e.to_string()),
                }
            }
            false => (self.shared.local_store().cas_data_path(function, &hash), 0),
        };
        match tokio::fs::read(&path).await {
            Ok(data) => {
                let _ = self.shared.local_store().record_access(&key, now).await;
                reapi::batch_read_blobs_response::Response {
                    digest: Some(digest.clone()),
                    data,
                    compressor,
                    status: None,
                }
            }
            Err(e) => failure(tonic::Code::Internal, format!("store i/o: {e}")),
        }
    }

    /// Look up one tree node during GetTree traversal. Stored Directory bytes
    /// are trusted, but their *child* digests are client data: anything odd
    /// down there is a miss, never a request error.
    async fn tree_node(
        &self,
        digest: &reapi::Digest,
        digest_function: i32,
    ) -> Result<Option<Vec<u8>>, tonic::Status> {
        if digest.size_bytes < 0 {
            return Ok(None);
        }
        let Some(hash) = normalize_hash(&digest.hash) else {
            return Ok(None);
        };
        let Some((function, meta)) = self.shared.local_store().cas_lookup(&hash, digest_function) else {
            return Ok(None);
        };
        if meta.size_bytes != digest.size_bytes as u64 {
            return Ok(None);
        }
        let bytes = tokio::fs::read(self.shared.local_store().cas_data_path(function, &hash))
            .await
            .map_err(|e| tonic::Status::internal(format!("store i/o: {e}")))?;
        self.shared.local_store().touch(
            &EntryKey {
                kind: EntryKind::Cas,
                function,
                hash,
            },
            now_unix(),
        );
        Ok(Some(bytes))
    }
}

#[tonic::async_trait]
impl reapi::action_cache_server::ActionCache for Api {
    async fn get_action_result(
        &self,
        request: tonic::Request<reapi::GetActionResultRequest>,
    ) -> Result<tonic::Response<reapi::ActionResult>, tonic::Status> {
        let hints = request_hints(&request);
        if let Some((mux, backend)) = self.shared.mux_route(&hints) {
            return mux.forward_get_action_result(backend, request).await;
        }
        let req = request.into_inner();
        let Some(action_digest) = &req.action_digest else {
            return Err(tonic::Status::invalid_argument("get_action_result without a digest"));
        };
        tracing::debug!(hash = %action_digest.hash, hints = %hints_summary(&hints), "get_action_result");
        let Some(hash) = normalize_hash(&action_digest.hash) else {
            return Err(tonic::Status::invalid_argument("malformed action digest"));
        };
        // NOTE: inline_stdout/inline_stderr/inline_output_files are ignored:
        // the stored result is returned verbatim and clients fetch output
        // blobs themselves (inlining is a SHOULD, and every client falls back).
        let Some((function, _)) = self.shared.local_store().ac_lookup(&hash, req.digest_function) else {
            tracing::debug!("get_action_result miss");
            return Err(tonic::Status::not_found("action result missing"));
        };
        tracing::debug!("get_action_result hit");
        let bytes = tokio::fs::read(self.shared.local_store().ac_data_path(function, &hash))
            .await
            .map_err(|_| tonic::Status::not_found("action result missing"))?;
        let result = match reapi::ActionResult::decode(&bytes[..]) {
            Ok(result) => result,
            Err(e) => {
                // Self-heal: bytes that no longer decode cannot have come
                // from a verified write; drop the entry.
                tracing::warn!(hash = %hash, error = %e, "dropping corrupt action result");
                self.shared
                    .local_store()
                    .remove(&EntryKey {
                        kind: EntryKind::ActionCache,
                        function,
                        hash: hash.clone(),
                    })
                    .await;
                return Err(tonic::Status::not_found("action result missing"));
            }
        };
        let _ = self
            .shared
            .local_store()
            .record_access(
                &EntryKey {
                    kind: EntryKind::ActionCache,
                    function,
                    hash,
                },
                now_unix(),
            )
            .await;
        Ok(tonic::Response::new(result))
    }

    async fn update_action_result(
        &self,
        request: tonic::Request<reapi::UpdateActionResultRequest>,
    ) -> Result<tonic::Response<reapi::ActionResult>, tonic::Status> {
        let hints = request_hints(&request);
        if let Some((mux, backend)) = self.shared.mux_route(&hints) {
            return mux.forward_update_action_result(backend, request).await;
        }
        let req = request.into_inner();
        let Some(action_digest) = &req.action_digest else {
            return Err(tonic::Status::invalid_argument("update_action_result without a digest"));
        };
        tracing::debug!(hash = %action_digest.hash, hints = %hints_summary(&hints), "update_action_result");
        let Some(action_result) = req.action_result else {
            return Err(tonic::Status::invalid_argument("update_action_result without a result"));
        };
        let mut bytes = Vec::new();
        action_result
            .encode(&mut bytes)
            .map_err(|e| tonic::Status::internal(format!("encode action result: {e}")))?;
        self.shared
            .local_store()
            .ac_put(&action_digest.hash, req.digest_function, &bytes, &hints, now_unix())
            .await
            .map_err(store_status)?;
        self.shared.evict_pass().await;
        Ok(tonic::Response::new(action_result))
    }
}

#[tonic::async_trait]
impl reapi::capabilities_server::Capabilities for Api {
    async fn get_capabilities(
        &self,
        _request: tonic::Request<reapi::GetCapabilitiesRequest>,
    ) -> Result<tonic::Response<reapi::ServerCapabilities>, tonic::Status> {
        // Deliberately never forwarded: clients get one stable answer from
        // the mux's own config no matter which backends are up. Operators
        // should keep these limits at or under every backend's.
        tracing::debug!("get_capabilities");
        // No ExecutionCapabilities: this server never executes. Clients pair
        // these cache endpoints with local execution.
        Ok(tonic::Response::new(reapi::ServerCapabilities {
            cache_capabilities: Some(reapi::CacheCapabilities {
                digest_functions: self.shared.config.proto_digest_functions(),
                action_cache_update_capabilities: Some(reapi::ActionCacheUpdateCapabilities {
                    update_enabled: true,
                }),
                cache_priority_capabilities: None,
                max_batch_total_size_bytes: self.shared.config.max_batch_bytes as i64,
                // Absolute symlinks in cached results are the executor's
                // problem; permitting them keeps cacheable results cacheable.
                symlink_absolute_path_strategy: 2, // Allowed
                supported_compressors: vec![1], // Zstd
                supported_batch_update_compressors: vec![1], // Zstd
                max_cas_blob_size_bytes: self.shared.config.max_blob_bytes.min(i64::MAX as u64) as i64,
                split_blob_support: false,
                splice_blob_support: false,
                fast_cdc_2020_params: None,
                rep_max_cdc_params: None,
            }),
            execution_capabilities: None,
            deprecated_api_version: None,
            low_api_version: Some(semver::SemVer {
                major: 2,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
            }),
            high_api_version: Some(semver::SemVer {
                major: 2,
                minor: 2,
                patch: 0,
                prerelease: String::new(),
            }),
        }))
    }
}

#[tonic::async_trait]
impl bytestream::byte_stream_server::ByteStream for Api {
    type ReadStream = tokio_stream::wrappers::ReceiverStream<Result<bytestream::ReadResponse, tonic::Status>>;

    async fn read(
        &self,
        request: tonic::Request<bytestream::ReadRequest>,
    ) -> Result<tonic::Response<Self::ReadStream>, tonic::Status> {
        let hints = request_hints(&request);
        if let Some((mux, backend)) = self.shared.mux_route(&hints) {
            return mux.forward_read(backend, request).await;
        }
        let req = request.into_inner();
        tracing::debug!(resource = %req.resource_name, hints = %hints_summary(&hints), "read");
        let resource = parse_resource_name(&req.resource_name)?;
        if resource.upload_id.is_some() {
            return Err(tonic::Status::invalid_argument("cannot read an uploads/ resource"));
        }
        let now = now_unix();
        let Some((function, meta)) = self.shared.local_store().cas_lookup(&resource.hash, 0) else {
            return Err(tonic::Status::not_found(format!("blob {:?} missing", resource.hash)));
        };
        if meta.size_bytes != resource.size_bytes {
            return Err(tonic::Status::not_found(format!("blob {:?} missing", resource.hash)));
        }
        if req.read_offset < 0 || req.read_offset as u64 > meta.size_bytes {
            return Err(tonic::Status::out_of_range("read_offset past end of blob"));
        }
        // Compressed reads stream the stored form, whose length differs
        // from the decoded size: offsets/limits (defined over decoded bytes)
        // cannot apply, so compressed reads are full-blob only.
        if resource.compressed && (req.read_offset != 0 || req.read_limit > 0) {
            return Err(tonic::Status::invalid_argument(
                "offset/limit reads of compressed blobs are not supported; read uncompressed",
            ));
        }
        // The empty blob synthesizes without a file: no chunks uncompressed,
        // one zstd-of-empty chunk compressed.
        if meta.size_bytes == 0 {
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            if resource.compressed {
                let _ = tx.send(Ok(bytestream::ReadResponse { data: zstd_empty() })).await;
            }
            return Ok(tonic::Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)));
        }
        let path = match resource.compressed {
            true => self
                .shared
                .local_store()
                .cas_compressed_path_for(function, &resource.hash, now)
                .await
                .map_err(store_status)?
                .ok_or_else(|| tonic::Status::not_found("blob missing"))?,
            false => self.shared.local_store().cas_data_path(function, &resource.hash),
        };
        // Best-effort like every other read path: a sidecar hiccup must not
        // fail a read of bytes that exist.
        let _ = self
            .shared
            .local_store()
            .record_access(
                &EntryKey {
                    kind: EntryKind::Cas,
                    function,
                    hash: resource.hash.clone(),
                },
                now,
            )
            .await;
        // Stream 64 KiB chunks from a spawned reader; the first error ends
        // the stream after one Err message.
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let mut remaining = match resource.compressed {
            true => u64::MAX,
            false => match req.read_limit <= 0 {
                true => u64::MAX,
                false => req.read_limit as u64,
            }
            .min(meta.size_bytes.saturating_sub(req.read_offset as u64)),
        };
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            use tokio::io::AsyncSeekExt;
            let mut file = match tokio::fs::File::open(&path).await {
                Ok(file) => file,
                Err(e) => {
                    let _ = tx.send(Err(tonic::Status::internal(format!("store i/o: {e}")))).await;
                    return;
                }
            };
            if req.read_offset != 0
                && let Err(e) = file.seek(std::io::SeekFrom::Start(req.read_offset as u64)).await
            {
                let _ = tx.send(Err(tonic::Status::internal(format!("store i/o: {e}")))).await;
                return;
            }
            let mut buf = vec![0u8; 64 * 1024];
            while remaining > 0 {
                let want = buf.len().min(remaining as usize);
                let n = match file.read(&mut buf[..want]).await {
                    Ok(n) => n,
                    Err(e) => {
                        let _ = tx.send(Err(tonic::Status::internal(format!("store i/o: {e}")))).await;
                        return;
                    }
                };
                if n == 0 {
                    break;
                }
                remaining -= n as u64;
                if tx.send(Ok(bytestream::ReadResponse { data: buf[..n].to_vec() })).await.is_err() {
                    break;
                }
            }
        });
        Ok(tonic::Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    async fn write(
        &self,
        request: tonic::Request<tonic::Streaming<bytestream::WriteRequest>>,
    ) -> Result<tonic::Response<bytestream::WriteResponse>, tonic::Status> {
        let hints = request_hints(&request);
        if let Some((mux, backend)) = self.shared.mux_route(&hints) {
            return mux.forward_write(backend, request).await;
        }
        let mut stream = request.into_inner();
        // The first message carries the resource name and opens the upload.
        let first = match stream.message().await.map_err(|e| {
            tonic::Status::unknown(format!("write stream failed: {}", e.message()))
        })? {
            Some(first) => first,
            None => return Err(tonic::Status::invalid_argument("empty write stream")),
        };
        if first.resource_name.is_empty() {
            return Err(tonic::Status::invalid_argument("first write lacks a resource name"));
        }
        let resource = parse_resource_name(&first.resource_name)?;
        tracing::debug!(
            resource = %first.resource_name,
            hints = %hints_summary(&hints),
            "write"
        );
        let id = match &resource.upload_id {
            Some(id) => id.clone(),
            None => self.shared.local_store().fresh_upload_id(),
        };
        let committed = self
            .shared
            .local_store()
            .upload_begin(
                &id,
                UploadExpect {
                    hash: resource.hash.clone(),
                    size_bytes: resource.size_bytes,
                    proto_function: 0,
                    compressed: resource.compressed,
                },
                &hints,
            )
            .await
            .map_err(store_status)?;
        if first.write_offset < 0 || first.write_offset as u64 != committed {
            if resource.upload_id.is_none() {
                self.shared.local_store().upload_abort(&id).await;
            }
            return Err(tonic::Status::invalid_argument(format!(
                "first write offset {} but {} bytes committed",
                first.write_offset, committed
            )));
        }
        let first_resource_name = first.resource_name.clone();
        let mut request = first;
        loop {
            if !request.resource_name.is_empty() && request.resource_name != first_resource_name {
                self.shared.local_store().upload_abort(&id).await;
                return Err(tonic::Status::invalid_argument("resource name changed mid-write"));
            }
            if request.write_offset < 0 {
                self.shared.local_store().upload_abort(&id).await;
                return Err(tonic::Status::invalid_argument("negative write offset"));
            }
            let committed = self
                .shared
                .local_store()
                .upload_append(&id, request.write_offset as u64, &request.data)
                .await
                .map_err(store_status)?;
            let finished = request.finish_write;
            match stream.message().await.map_err(|e| {
                tonic::Status::unknown(format!("write stream failed: {}", e.message()))
            })? {
                Some(next) => {
                    if finished {
                        self.shared.local_store().upload_abort(&id).await;
                        return Err(tonic::Status::invalid_argument("data after finish_write"));
                    }
                    request = next;
                }
                None => {
                    // Client closed the stream: commit iff finished, else
                    // leave the spool for a resumed Write.
                    if !finished {
                        return Ok(tonic::Response::new(bytestream::WriteResponse {
                            committed_size: committed as i64,
                        }));
                    }
                    let (_, total) =
                        self.shared.local_store().upload_finish(&id, now_unix()).await.map_err(store_status)?;
                    tracing::debug!(bytes = total, "write committed");
                    self.shared.evict_pass().await;
                    return Ok(tonic::Response::new(bytestream::WriteResponse {
                        committed_size: total as i64,
                    }));
                }
            }
        }
    }

    async fn query_write_status(
        &self,
        request: tonic::Request<bytestream::QueryWriteStatusRequest>,
    ) -> Result<tonic::Response<bytestream::QueryWriteStatusResponse>, tonic::Status> {
        // Hints matter here too: a resumed upload lives on the pinned
        // backend, so status queries must route to the same one.
        let hints = request_hints(&request);
        if let Some((mux, backend)) = self.shared.mux_route(&hints) {
            return mux.forward_query_write_status(backend, request).await;
        }
        let req = request.into_inner();
        tracing::debug!(resource = %req.resource_name, "query_write_status");
        let resource = parse_resource_name(&req.resource_name)?;
        let Some(id) = resource.upload_id else {
            return Err(tonic::Status::not_found("not a resumable upload"));
        };
        match self.shared.local_store().upload_committed(&id).await {
            Some(committed) => Ok(tonic::Response::new(bytestream::QueryWriteStatusResponse {
                committed_size: committed as i64,
                complete: false,
            })),
            // Finished uploads leave no state behind; if the bytes made it
            // to CAS, the upload is complete.
            None => match self.shared.local_store().cas_lookup(&resource.hash, 0) {
                Some((_, meta)) if meta.size_bytes == resource.size_bytes => {
                    Ok(tonic::Response::new(bytestream::QueryWriteStatusResponse {
                        committed_size: resource.size_bytes as i64,
                        complete: true,
                    }))
                }
                _ => Err(tonic::Status::not_found("unknown upload")),
            },
        }
    }
}

// ---- serving ----

/// gRPC frame limit: admits the largest configured batch (handlers
/// enforce the documented caps per RPC). Shared by the server and the mux's
/// backend clients so forwarded messages fit both directions.
pub fn frame_bytes(max_batch_bytes: u64) -> usize {
    max_batch_bytes.max(64 << 20).min(i32::MAX as u64) as usize
}

/// Serve all four APIs on an already-bound listener until it fails.
pub async fn serve(listener: tokio::net::TcpListener, shared: Arc<Shared>) -> Result<(), tonic::transport::Error> {
    let frame = frame_bytes(shared.config.max_batch_bytes);
    let api = Api::new(shared);
    tonic::transport::Server::builder()
        .add_service(
            reapi::content_addressable_storage_server::ContentAddressableStorageServer::new(
                api.clone(),
            )
            .max_decoding_message_size(frame)
            .max_encoding_message_size(frame),
        )
        .add_service(
            reapi::action_cache_server::ActionCacheServer::new(api.clone())
                .max_decoding_message_size(frame)
                .max_encoding_message_size(frame),
        )
        .add_service(
            reapi::capabilities_server::CapabilitiesServer::new(api.clone())
                .max_decoding_message_size(frame)
                .max_encoding_message_size(frame),
        )
        .add_service(
            bytestream::byte_stream_server::ByteStreamServer::new(api)
                .max_decoding_message_size(frame)
                .max_encoding_message_size(frame),
        )
        .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_names() {
        let hash = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        // Plain, with and without instance prefix.
        assert_eq!(
            parse_resource_name(&format!("blobs/{hash}/3")).unwrap(),
            ResourceName {
                upload_id: None,
                hash: hash.to_string(),
                size_bytes: 3,
                compressed: false,
            }
        );
        assert_eq!(
            parse_resource_name(&format!("main/blobs/{hash}/3")).unwrap().upload_id,
            None
        );
        assert_eq!(
            parse_resource_name(&format!("a/b/c/blobs/{hash}/3")).unwrap().size_bytes,
            3
        );
        // Resumable uploads.
        assert_eq!(
            parse_resource_name(&format!("uploads/uuid-1/blobs/{hash}/3")).unwrap(),
            ResourceName {
                upload_id: Some("uuid-1".to_string()),
                hash: hash.to_string(),
                size_bytes: 3,
                compressed: false,
            }
        );
        assert_eq!(
            parse_resource_name(&format!("main/uploads/uuid-1/blobs/{hash}/3")).unwrap().upload_id,
            Some("uuid-1".to_string())
        );
        // Compressed forms.
        let compressed =
            parse_resource_name(&format!("compressed-blobs/zstd/{hash}/3")).unwrap();
        assert!(compressed.compressed && compressed.upload_id.is_none());
        let compressed_up =
            parse_resource_name(&format!("uploads/u/compressed-blobs/zstd/{hash}/3")).unwrap();
        assert!(compressed_up.compressed);
        assert_eq!(compressed_up.upload_id.as_deref(), Some("u"));
        // Uppercase hashes normalize.
        assert_eq!(
            parse_resource_name(&format!("blobs/{}/3", hash.to_uppercase())).unwrap().hash,
            hash
        );
        // Malformed.
        for bad in [
            "",
            "blobs",
            "blobs/zz/3",
            &format!("blobs/{hash}"),
            &format!("blobs/{hash}/-3"),
            &format!("blobs/{hash}/x"),
            "uploads/blobs/abc/3",
            &format!("uploads/{hash}/blobs/x/3"),
            &format!("compressed-blobs/gzip/{hash}/3"),
            &format!("files/{hash}/3"),
        ] {
            assert!(parse_resource_name(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn metadata_extraction() {
        let mut meta = reapi::RequestMetadata {
            tool_invocation_id: "inv".to_string(),
            action_mnemonic: "Rustc".to_string(),
            target_id: "//foo:bar".to_string(),
            ..Default::default()
        };
        // Correlated ids ride along when present.
        meta.correlated_invocations_id = "corr".to_string();
        let mut bytes = Vec::new();
        meta.encode(&mut bytes).unwrap();
        let mut request = tonic::Request::new(());
        request.metadata_mut().insert_bin(
            "build.bazel.remote.execution.v2.requestmetadata-bin",
            tonic::metadata::MetadataValue::from_bytes(&bytes),
        );
        let hints = request_hints(&request);
        assert_eq!(hints.mnemonic.as_deref(), Some("Rustc"));
        assert_eq!(hints.target_id.as_deref(), Some("//foo:bar"));
        assert_eq!(hints.tool_invocation_id.as_deref(), Some("inv"));
        assert_eq!(hints.correlated_invocations_id.as_deref(), Some("corr"));
        assert!(hints.action_id.is_none());
        // Absent header: empty hints.
        assert!(request_hints(&tonic::Request::new(())).is_empty());
        // Corrupt header: empty hints, no error.
        let mut bad = tonic::Request::new(());
        bad.metadata_mut().insert_bin(
            "build.bazel.remote.execution.v2.requestmetadata-bin",
            tonic::metadata::MetadataValue::from_bytes(b"\xff\xff"),
        );
        assert!(request_hints(&bad).is_empty());
    }
}
