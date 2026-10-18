//! Multiplex mode: route each build to one of several `reapid` backends.
//!
//! Every instance serves from its `[[backends]]` list in priority order. A
//! build (identified by the `tool_invocation_id` request-metadata header)
//! pins to the first backend whose circuit breaker admits traffic, and that
//! pin never moves mid-build: if the pinned backend dies, the RPC fails with
//! `UNAVAILABLE` and buck2's restarter brings the build back under a fresh
//! invocation id, which routes around the dead backend. New builds always
//! re-select, so recovery needs no operator action.
//!
//! The `self` endpoint routes to this daemon's own store and is always
//! admissible, which makes it the offline fallback: list it last and builds
//! keep working (against the local cache) when every remote is unreachable.
//! Capabilities are answered locally, never forwarded, so clients see one
//! stable answer no matter which backends are up.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use crate::config::BackendConfig;

/// Health outcome of one forwarded RPC, from the mux's point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The backend answered (or the call cleanly ended): it is alive.
    Ok,
    /// The call never got an application answer: connect/refused/reset,
    /// timeout, or a retryable code from a backend that cannot serve.
    ConnectivityFailure,
}

/// Classify a backend RPC error for breaker health. Only transport-level
/// failures count: tonic funnels connect/refused/reset/h2 failures into
/// `Unknown`, and a backend that cannot serve (including another mux whose
/// own backends are all down) answers `Unavailable`/`DeadlineExceeded`.
///
/// Application errors prove the backend is alive and answering. In
/// particular a `reapid` backend only emits `Unknown` itself for a broken
/// inbound client stream, which a well-behaved forwarder never sends (our
/// bridged streams end cleanly), so `Unknown` from a backend is treated as
/// transport trouble.
pub fn classify(status: &tonic::Status) -> Outcome {
    match status.code() {
        tonic::Code::Unknown | tonic::Code::Unavailable | tonic::Code::DeadlineExceeded => {
            Outcome::ConnectivityFailure
        }
        _ => Outcome::Ok,
    }
}

/// Rewrite a backend error for the mux's own client: tag the backend name,
/// and normalize connectivity failures to `Unavailable` (the code buck2's
/// restarter recovers from) however the transport surfaced them.
fn backend_error(name: &str, status: tonic::Status) -> tonic::Status {
    let message = format!("backend {name}: {}", status.message());
    match classify(&status) {
        Outcome::ConnectivityFailure => tonic::Status::unavailable(message),
        Outcome::Ok => tonic::Status::new(status.code(), message),
    }
}

/// One remote backend's tonic clients. `Capabilities` is deliberately
/// absent: the mux answers those locally.
#[derive(Clone)]
struct Clients {
    cas: reapi::content_addressable_storage_client::ContentAddressableStorageClient<tonic::transport::Channel>,
    ac: reapi::action_cache_client::ActionCacheClient<tonic::transport::Channel>,
    bs: bytestream::byte_stream_client::ByteStreamClient<tonic::transport::Channel>,
}

/// Runtime state for one configured backend.
struct Backend {
    name: String,
    local: bool,
    clients: Option<Clients>,
    max_consecutive_failures: u32,
    cooldown: Duration,
    consecutive_failures: u32,
    /// Set while the breaker is open (tripped at this instant).
    opened_at: Option<Instant>,
}

impl Backend {
    /// Admissible for new builds: closed, or open but past its cooldown (a
    /// trial build is allowed through; success closes it, failure re-opens).
    fn admits(&self, now: Instant) -> bool {
        match self.opened_at {
            None => true,
            Some(opened) => now.duration_since(opened) >= self.cooldown,
        }
    }
}

/// A build's backend pin. Pins never expire on a timer (a build may run for
/// hours); the table is only pruned when it grows past [`MAX_PINS`], and
/// then only entries idle for over an hour.
struct Pin {
    backend: usize,
    last_seen: Instant,
}

const MAX_PINS: usize = 4096;
const PIN_PRUNE_AFTER: Duration = Duration::from_secs(3600);

struct State {
    backends: Vec<Backend>,
    pins: HashMap<String, Pin>,
}

/// The multiplexer. Cloneable handle over shared breaker + pin state.
#[derive(Clone)]
pub struct Mux {
    state: Arc<Mutex<State>>,
}

/// Where one RPC goes.
pub enum Route {
    /// Serve from this daemon's own store.
    Local,
    /// Forward to `backends[index]`.
    Remote(usize),
}

impl Mux {
    /// Build clients for the configured backends. Channels are lazy: an
    /// unreachable backend fails its first RPC, not construction, so a mux
    /// can start with every remote down.
    pub fn new(configs: &[BackendConfig], frame: usize) -> Result<Mux, String> {
        if configs.is_empty() {
            return Err("at least one backend is required".to_string());
        }
        let mut backends = Vec::with_capacity(configs.len());
        for config in configs {
            if config.is_self() {
                tracing::info!(backend = %config.name, "backend: self (local store)");
                backends.push(Backend {
                    name: config.name.clone(),
                    local: true,
                    clients: None,
                    max_consecutive_failures: config.max_consecutive_failures,
                    cooldown: Duration::from_secs(config.cooldown_secs),
                    consecutive_failures: 0,
                    opened_at: None,
                });
                continue;
            }
            let endpoint = tonic::transport::Endpoint::from_shared(config.endpoint.clone())
                .map_err(|e| format!("backend {:?}: bad endpoint: {e}", config.name))?;
            // Reachability is a connect-level property: bound setup, then
            // hold idle connections open with keepalives so a silently dead
            // peer surfaces as a failure rather than a hung stream.
            let channel = endpoint
                .connect_timeout(Duration::from_secs(5))
                .http2_keep_alive_interval(Duration::from_secs(30))
                .connect_lazy();
            tracing::info!(backend = %config.name, endpoint = %config.endpoint, "backend: remote");
            backends.push(Backend {
                name: config.name.clone(),
                local: false,
                clients: Some(Clients {
                    cas: reapi::content_addressable_storage_client::ContentAddressableStorageClient::new(
                        channel.clone(),
                    )
                    .max_decoding_message_size(frame)
                    .max_encoding_message_size(frame),
                    ac: reapi::action_cache_client::ActionCacheClient::new(channel.clone())
                        .max_decoding_message_size(frame)
                        .max_encoding_message_size(frame),
                    bs: bytestream::byte_stream_client::ByteStreamClient::new(channel)
                        .max_decoding_message_size(frame)
                        .max_encoding_message_size(frame),
                }),
                max_consecutive_failures: config.max_consecutive_failures,
                cooldown: Duration::from_secs(config.cooldown_secs),
                consecutive_failures: 0,
                opened_at: None,
            });
        }
        Ok(Mux {
            state: Arc::new(Mutex::new(State {
                backends,
                pins: HashMap::new(),
            })),
        })
    }

    /// Route one RPC. A pinned invocation always returns its backend, even
    /// past an open breaker: the pin never moves mid-build. An unpinned
    /// invocation (or a call without one, which is never pinned) takes the
    /// first admissible backend in priority order.
    pub fn route(&self, invocation: Option<&str>) -> Route {
        let mut state = self.state.lock().unwrap();
        if let Some(id) = invocation {
            let pinned = state.pins.get_mut(id).map(|pin| {
                pin.last_seen = Instant::now();
                pin.backend
            });
            if let Some(backend) = pinned {
                return route_for(&state.backends, backend);
            }
        }
        let now = Instant::now();
        // Highest-priority admissible backend wins. If every remote is
        // cooling down, the highest-priority one is attempted anyway: the
        // cooldown is a hint, and a recovered backend should serve
        // immediately rather than fail fast.
        let mut fallback = None;
        let mut selected = None;
        for (index, backend) in state.backends.iter().enumerate() {
            if backend.local || backend.admits(now) {
                selected = Some(index);
                break;
            }
            if fallback.is_none() {
                fallback = Some(index);
            }
        }
        let admissible = selected.is_some();
        let selected = selected.or(fallback).expect("mux with no backends");
        if admissible {
            tracing::debug!(backend = %state.backends[selected].name, "routed new build");
        } else {
            tracing::debug!(
                backend = %state.backends[selected].name,
                "all breakers open; attempting preferred backend anyway"
            );
        }
        if let Some(id) = invocation {
            if state.pins.len() >= MAX_PINS {
                state.pins.retain(|_, pin| pin.last_seen.elapsed() < PIN_PRUNE_AFTER);
            }
            state.pins.insert(
                id.to_string(),
                Pin {
                    backend: selected,
                    last_seen: now,
                },
            );
            tracing::debug!(
                backend = %state.backends[selected].name,
                invocation = %id,
                "pinned build"
            );
        }
        route_for(&state.backends, selected)
    }

    /// Record one forwarded RPC's outcome against the backend's breaker.
    /// Application answers (including errors) close the breaker; only
    /// connectivity failures open it.
    pub fn report(&self, index: usize, outcome: Outcome) {
        let mut state = self.state.lock().unwrap();
        let Some(backend) = state.backends.get_mut(index) else {
            return;
        };
        if backend.local {
            return;
        }
        match outcome {
            Outcome::Ok => {
                backend.consecutive_failures = 0;
                if backend.opened_at.take().is_some() {
                    tracing::info!(backend = %backend.name, "circuit breaker closed");
                }
            }
            Outcome::ConnectivityFailure => {
                backend.consecutive_failures += 1;
                if backend.consecutive_failures >= backend.max_consecutive_failures
                    && backend.opened_at.is_none()
                {
                    backend.opened_at = Some(Instant::now());
                    tracing::warn!(
                        backend = %backend.name,
                        failures = backend.consecutive_failures,
                        cooldown_secs = backend.cooldown.as_secs(),
                        "circuit breaker open; new builds route around this backend"
                    );
                } else if backend.opened_at.is_some() {
                    // A half-open trial failed: the cooldown restarts.
                    backend.opened_at = Some(Instant::now());
                }
            }
        }
    }

    fn clients(&self, index: usize) -> Clients {
        self.state.lock().unwrap().backends[index]
            .clients
            .clone()
            .expect("remote route without clients")
    }

    fn backend_name(&self, index: usize) -> String {
        self.state.lock().unwrap().backends[index].name.clone()
    }

    /// Forward one unary RPC, reporting its outcome.
    async fn unary<Req, Resp, F, Fut>(
        &self,
        index: usize,
        rpc: &'static str,
        request: tonic::Request<Req>,
        call: F,
    ) -> Result<tonic::Response<Resp>, tonic::Status>
    where
        F: FnOnce(tonic::Request<Req>) -> Fut,
        Fut: std::future::Future<Output = Result<tonic::Response<Resp>, tonic::Status>>,
    {
        let name = self.backend_name(index);
        tracing::debug!(backend = %name, rpc, "forward");
        match call(request).await {
            Ok(response) => {
                self.report(index, Outcome::Ok);
                Ok(response)
            }
            Err(status) => {
                self.report(index, classify(&status));
                Err(backend_error(&name, status))
            }
        }
    }

    /// Forward one server-streaming RPC. The backend stream is pumped into a
    /// channel so terminal health reports exactly once however it ends; the
    /// caller sees the backend's messages verbatim.
    async fn server_stream<Req, Resp, F, Fut>(
        &self,
        index: usize,
        rpc: &'static str,
        request: tonic::Request<Req>,
        call: F,
    ) -> Result<
        tonic::Response<tokio_stream::wrappers::ReceiverStream<Result<Resp, tonic::Status>>>,
        tonic::Status,
    >
    where
        F: FnOnce(tonic::Request<Req>) -> Fut,
        Fut: std::future::Future<
            Output = Result<tonic::Response<tonic::Streaming<Resp>>, tonic::Status>,
        >,
        Resp: Send + 'static,
    {
        let name = self.backend_name(index);
        tracing::debug!(backend = %name, rpc, "forward");
        let response = match call(request).await {
            Ok(response) => response,
            Err(status) => {
                self.report(index, classify(&status));
                return Err(backend_error(&name, status));
            }
        };
        let mut inbound = response.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let mux = self.clone();
        tokio::spawn(async move {
            loop {
                match inbound.message().await {
                    Ok(Some(message)) => {
                        if tx.send(Ok(message)).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        mux.report(index, Outcome::Ok);
                        break;
                    }
                    Err(status) => {
                        mux.report(index, classify(&status));
                        let _ = tx.send(Err(backend_error(&name, status))).await;
                        break;
                    }
                }
            }
        });
        Ok(tonic::Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    /// Forward one client-streaming RPC. The inbound stream is bridged
    /// message-by-message (never buffered whole); if the client's own
    /// stream breaks, that error is returned exactly as the local path
    /// would, while backend health still follows the backend's answer.
    async fn client_stream<Req, Resp, F, Fut>(
        &self,
        index: usize,
        rpc: &'static str,
        request: tonic::Request<tonic::Streaming<Req>>,
        call: F,
    ) -> Result<tonic::Response<Resp>, tonic::Status>
    where
        Req: Send + 'static,
        F: FnOnce(tonic::Request<tokio_stream::wrappers::ReceiverStream<Req>>) -> Fut,
        Fut: std::future::Future<Output = Result<tonic::Response<Resp>, tonic::Status>>,
    {
        let name = self.backend_name(index);
        tracing::debug!(backend = %name, rpc, "forward");
        let (metadata, extensions, mut inbound) = request.into_parts();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let pump_failed = Arc::new(Mutex::new(None));
        let pump_failed_task = pump_failed.clone();
        tokio::spawn(async move {
            loop {
                match inbound.message().await {
                    Ok(Some(message)) => {
                        if tx.send(message).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        *pump_failed_task.lock().unwrap() = Some(tonic::Status::unknown(format!(
                            "client stream failed: {}",
                            e.message()
                        )));
                        break;
                    }
                }
            }
        });
        let mut outbound =
            tonic::Request::new(tokio_stream::wrappers::ReceiverStream::new(rx));
        *outbound.metadata_mut() = metadata;
        *outbound.extensions_mut() = extensions;
        let response = match call(outbound).await {
            Ok(response) => {
                self.report(index, Outcome::Ok);
                response
            }
            Err(status) => {
                self.report(index, classify(&status));
                return match pump_failed.lock().unwrap().take() {
                    Some(pump) => Err(pump),
                    None => Err(backend_error(&name, status)),
                };
            }
        };
        // The backend answered but our own client's stream broke mid-flight:
        // report the client's failure, as the local path would.
        match pump_failed.lock().unwrap().take() {
            Some(pump) => Err(pump),
            None => Ok(response),
        }
    }

    pub async fn forward_find_missing_blobs(
        &self,
        index: usize,
        request: tonic::Request<reapi::FindMissingBlobsRequest>,
    ) -> Result<tonic::Response<reapi::FindMissingBlobsResponse>, tonic::Status> {
        let mut client = self.clients(index).cas;
        self.unary(index, "find_missing_blobs", request, |request| {
            client.find_missing_blobs(request)
        })
        .await
    }

    pub async fn forward_batch_update_blobs(
        &self,
        index: usize,
        request: tonic::Request<reapi::BatchUpdateBlobsRequest>,
    ) -> Result<tonic::Response<reapi::BatchUpdateBlobsResponse>, tonic::Status> {
        let mut client = self.clients(index).cas;
        self.unary(index, "batch_update_blobs", request, |request| {
            client.batch_update_blobs(request)
        })
        .await
    }

    pub async fn forward_batch_read_blobs(
        &self,
        index: usize,
        request: tonic::Request<reapi::BatchReadBlobsRequest>,
    ) -> Result<tonic::Response<reapi::BatchReadBlobsResponse>, tonic::Status> {
        let mut client = self.clients(index).cas;
        self.unary(index, "batch_read_blobs", request, |request| {
            client.batch_read_blobs(request)
        })
        .await
    }

    pub async fn forward_get_tree(
        &self,
        index: usize,
        request: tonic::Request<reapi::GetTreeRequest>,
    ) -> Result<
        tonic::Response<tokio_stream::wrappers::ReceiverStream<Result<reapi::GetTreeResponse, tonic::Status>>>,
        tonic::Status,
    > {
        let mut client = self.clients(index).cas;
        self.server_stream(index, "get_tree", request, |request| client.get_tree(request))
            .await
    }

    pub async fn forward_split_blob(
        &self,
        index: usize,
        request: tonic::Request<reapi::SplitBlobRequest>,
    ) -> Result<tonic::Response<reapi::SplitBlobResponse>, tonic::Status> {
        let mut client = self.clients(index).cas;
        self.unary(index, "split_blob", request, |request| client.split_blob(request))
            .await
    }

    pub async fn forward_splice_blob(
        &self,
        index: usize,
        request: tonic::Request<reapi::SpliceBlobRequest>,
    ) -> Result<tonic::Response<reapi::SpliceBlobResponse>, tonic::Status> {
        let mut client = self.clients(index).cas;
        self.unary(index, "splice_blob", request, |request| client.splice_blob(request))
            .await
    }

    pub async fn forward_get_chunk_mapping(
        &self,
        index: usize,
        request: tonic::Request<reapi::GetChunkMappingRequest>,
    ) -> Result<
        tonic::Response<
            tokio_stream::wrappers::ReceiverStream<Result<reapi::GetChunkMappingResponse, tonic::Status>>,
        >,
        tonic::Status,
    > {
        let mut client = self.clients(index).cas;
        self.server_stream(index, "get_chunk_mapping", request, |request| {
            client.get_chunk_mapping(request)
        })
        .await
    }

    pub async fn forward_register_chunk_mapping(
        &self,
        index: usize,
        request: tonic::Request<tonic::Streaming<reapi::RegisterChunkMappingRequest>>,
    ) -> Result<tonic::Response<reapi::RegisterChunkMappingResponse>, tonic::Status> {
        let mut client = self.clients(index).cas;
        self.client_stream(index, "register_chunk_mapping", request, |request| {
            client.register_chunk_mapping(request)
        })
        .await
    }

    pub async fn forward_get_action_result(
        &self,
        index: usize,
        request: tonic::Request<reapi::GetActionResultRequest>,
    ) -> Result<tonic::Response<reapi::ActionResult>, tonic::Status> {
        let mut client = self.clients(index).ac;
        self.unary(index, "get_action_result", request, |request| {
            client.get_action_result(request)
        })
        .await
    }

    pub async fn forward_update_action_result(
        &self,
        index: usize,
        request: tonic::Request<reapi::UpdateActionResultRequest>,
    ) -> Result<tonic::Response<reapi::ActionResult>, tonic::Status> {
        let mut client = self.clients(index).ac;
        self.unary(index, "update_action_result", request, |request| {
            client.update_action_result(request)
        })
        .await
    }

    pub async fn forward_read(
        &self,
        index: usize,
        request: tonic::Request<bytestream::ReadRequest>,
    ) -> Result<
        tonic::Response<tokio_stream::wrappers::ReceiverStream<Result<bytestream::ReadResponse, tonic::Status>>>,
        tonic::Status,
    > {
        let mut client = self.clients(index).bs;
        self.server_stream(index, "read", request, |request| client.read(request))
            .await
    }

    pub async fn forward_write(
        &self,
        index: usize,
        request: tonic::Request<tonic::Streaming<bytestream::WriteRequest>>,
    ) -> Result<tonic::Response<bytestream::WriteResponse>, tonic::Status> {
        let mut client = self.clients(index).bs;
        self.client_stream(index, "write", request, |request| client.write(request))
            .await
    }

    pub async fn forward_query_write_status(
        &self,
        index: usize,
        request: tonic::Request<bytestream::QueryWriteStatusRequest>,
    ) -> Result<tonic::Response<bytestream::QueryWriteStatusResponse>, tonic::Status> {
        let mut client = self.clients(index).bs;
        self.unary(index, "query_write_status", request, |request| {
            client.query_write_status(request)
        })
        .await
    }
}

fn route_for(backends: &[Backend], index: usize) -> Route {
    match backends[index].local {
        true => Route::Local,
        false => Route::Remote(index),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(name: &str, endpoint: &str) -> BackendConfig {
        BackendConfig {
            name: name.to_string(),
            endpoint: endpoint.to_string(),
            max_consecutive_failures: 2,
            cooldown_secs: 3600,
        }
    }

    fn mux(configs: &[BackendConfig]) -> Mux {
        Mux::new(configs, 4 << 20).unwrap()
    }

    fn route_index(mux: &Mux, invocation: Option<&str>) -> Option<usize> {
        match mux.route(invocation) {
            Route::Local => None,
            Route::Remote(index) => Some(index),
        }
    }

    #[test]
    fn classify_splits_transport_from_application() {
        for code in [tonic::Code::Unknown, tonic::Code::Unavailable, tonic::Code::DeadlineExceeded] {
            assert_eq!(classify(&tonic::Status::new(code, "x")), Outcome::ConnectivityFailure);
        }
        for code in [
            tonic::Code::Ok,
            tonic::Code::NotFound,
            tonic::Code::InvalidArgument,
            tonic::Code::Internal,
            tonic::Code::FailedPrecondition,
            tonic::Code::Unimplemented,
            tonic::Code::ResourceExhausted,
            tonic::Code::Cancelled,
        ] {
            assert_eq!(classify(&tonic::Status::new(code, "x")), Outcome::Ok, "{code:?}");
        }
    }

    #[test]
    fn backend_errors_tag_name_and_normalize_connectivity() {
        let err = backend_error("office", tonic::Status::unknown("error trying to connect"));
        assert_eq!(err.code(), tonic::Code::Unavailable);
        assert!(err.message().starts_with("backend office: "), "{}", err.message());
        let err = backend_error("office", tonic::Status::not_found("missing"));
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert!(err.message().starts_with("backend office: "), "{}", err.message());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn prefers_first_admissible_backend() {
        let mux = mux(&[
            backend("a", "http://127.0.0.1:9"),
            backend("b", "http://127.0.0.1:9"),
        ]);
        assert_eq!(route_index(&mux, Some("build-1")), Some(0));
        // A new build also takes the preferred backend while it is healthy.
        assert_eq!(route_index(&mux, Some("build-2")), Some(0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn pins_never_move_mid_build() {
        let mux = mux(&[
            backend("a", "http://127.0.0.1:9"),
            backend("b", "http://127.0.0.1:9"),
        ]);
        assert_eq!(route_index(&mux, Some("build-1")), Some(0));
        // Trip a's breaker: the pinned build stays on a (its RPCs will fail
        // loudly), while new builds route around it.
        mux.report(0, Outcome::ConnectivityFailure);
        mux.report(0, Outcome::ConnectivityFailure);
        assert_eq!(route_index(&mux, Some("build-1")), Some(0));
        assert_eq!(route_index(&mux, Some("build-2")), Some(1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn successes_close_the_breaker() {
        let mux = mux(&[backend("a", "http://127.0.0.1:9"), backend("b", "http://127.0.0.1:9")]);
        // One failure is not enough to open (threshold 2).
        mux.report(0, Outcome::ConnectivityFailure);
        assert_eq!(route_index(&mux, Some("build-1")), Some(0));
        // An application answer resets the count.
        mux.report(0, Outcome::Ok);
        mux.report(0, Outcome::ConnectivityFailure);
        assert_eq!(route_index(&mux, Some("build-2")), Some(0));
        // Two in a row opens; the next success (a half-open trial, or a
        // pinned build's RPC landing after recovery) closes again.
        mux.report(0, Outcome::ConnectivityFailure);
        assert_eq!(route_index(&mux, Some("build-3")), Some(1));
        mux.report(0, Outcome::Ok);
        assert_eq!(route_index(&mux, Some("build-4")), Some(0));
    }

    #[test]
    fn cooldown_admits_trials() {
        let backend = Backend {
            name: "a".to_string(),
            local: false,
            clients: None,
            max_consecutive_failures: 1,
            cooldown: Duration::from_secs(60),
            consecutive_failures: 1,
            opened_at: Some(Instant::now()),
        };
        assert!(!backend.admits(Instant::now()));
        assert!(backend.admits(Instant::now() + Duration::from_secs(61)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn all_cooling_falls_back_to_preferred() {
        let mux = mux(&[backend("a", "http://127.0.0.1:9"), backend("b", "http://127.0.0.1:9")]);
        for index in [0, 1] {
            mux.report(index, Outcome::ConnectivityFailure);
            mux.report(index, Outcome::ConnectivityFailure);
        }
        // Both breakers open: attempt the preferred backend anyway rather
        // than failing fast, so a silent recovery serves immediately.
        assert_eq!(route_index(&mux, Some("build-1")), Some(0));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn self_is_always_admissible() {
        let mux = mux(&[backend("a", "http://127.0.0.1:9"), backend("local", "self")]);
        assert_eq!(route_index(&mux, Some("build-1")), Some(0));
        mux.report(0, Outcome::ConnectivityFailure);
        mux.report(0, Outcome::ConnectivityFailure);
        assert!(matches!(mux.route(Some("build-2")), Route::Local));
        // ... and an unpinned call without an invocation id takes it too.
        assert!(matches!(mux.route(None), Route::Local));
    }
}
