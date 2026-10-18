//! Multiplex-mode integration tests: a `reapid` fronting other `reapid`s.
//!
//! Each test spins real backend daemons plus a mux on ephemeral loopback
//! ports and drives the mux with the generated tonic clients. The behaviors
//! pinned here are the mux's contract: priority routing, per-build pins
//! that never move mid-build, circuit-breaker failover between builds,
//! `self` as the always-reachable fallback, and metadata forwarding.

use std::sync::Arc;

use reapi_cache::config::Config;
use reapi_cache::digest::DigestFunction;
use reapi_cache::services::Shared;
use reapi_cache::store::Store;

type CasClient = reapi::content_addressable_storage_client::ContentAddressableStorageClient<tonic::transport::Channel>;
type BsClient = bytestream::byte_stream_client::ByteStreamClient<tonic::transport::Channel>;
type CapsClient = reapi::capabilities_client::CapabilitiesClient<tonic::transport::Channel>;

/// One running daemon (backend or mux).
struct Instance {
    _temp: tempfile::TempDir,
    store_dir: Option<std::path::PathBuf>,
    addr: std::net::SocketAddr,
    _handle: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}

impl Instance {
    /// Spawn a daemon. `cache` decides whether it has a `store_dir` (and
    /// hence may list `self`); `backends` is the raw `[[backends]]` TOML.
    async fn spawn(cache: bool, backends: &str) -> Instance {
        let temp = tempfile::TempDir::new().unwrap();
        let (store_line, store_dir) = match cache {
            true => {
                let dir = temp.path().join("store");
                (format!("store_dir = \"{}\"\n", dir.display()), Some(dir))
            }
            false => (String::new(), None),
        };
        let config =
            Config::from_toml(&format!("bind = \"127.0.0.1:0\"\n{store_line}{backends}")).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let store = match &store_dir {
            Some(dir) => Some(Store::open(dir.clone(), config.max_blob_bytes, 1_700_000_000).await.unwrap()),
            None => None,
        };
        let mux = reapi_cache::mux::Mux::new(
            &config.backends,
            reapi_cache::services::frame_bytes(config.max_batch_bytes),
        )
        .unwrap();
        let shared = Arc::new(Shared::new(store, config, mux));
        let _handle = tokio::spawn(reapi_cache::services::serve(listener, shared));
        Instance {
            _temp: temp,
            store_dir,
            addr,
            _handle,
        }
    }

    async fn backend() -> Instance {
        Instance::spawn(true, "[[backends]]\nname = \"local\"\nendpoint = \"self\"\n").await
    }

    fn endpoint(&self) -> String {
        format!("http://{}", self.addr)
    }

    async fn cas(&self) -> CasClient {
        CasClient::connect(self.endpoint()).await.unwrap()
    }

    async fn bs(&self) -> BsClient {
        BsClient::connect(self.endpoint()).await.unwrap()
    }

    async fn caps(&self) -> CapsClient {
        CapsClient::connect(self.endpoint()).await.unwrap()
    }

    fn remote_toml(&self, name: &str, extra: &str) -> String {
        format!("[[backends]]\nname = \"{name}\"\nendpoint = \"{}\"\n{extra}", self.endpoint())
    }
}

fn digest(data: &[u8]) -> reapi::Digest {
    reapi::Digest {
        hash: DigestFunction::Sha256.hexdigest(data),
        size_bytes: data.len() as i64,
    }
}

const METADATA_HEADER: &str = "build.bazel.remote.execution.v2.requestmetadata-bin";

/// Attach request metadata (the build uuid, plus descriptive fields) to an
/// outbound RPC, as buck2 does.
fn with_metadata<T>(msg: T, invocation: &str, mnemonic: &str, target: &str) -> tonic::Request<T> {
    let meta = reapi::RequestMetadata {
        tool_invocation_id: invocation.to_string(),
        action_mnemonic: mnemonic.to_string(),
        target_id: target.to_string(),
        ..Default::default()
    };
    let mut bytes = Vec::new();
    prost::Message::encode(&meta, &mut bytes).unwrap();
    let mut request = tonic::Request::new(msg);
    request.metadata_mut().insert_bin(
        METADATA_HEADER,
        tonic::metadata::MetadataValue::from_bytes(&bytes),
    );
    request
}

fn with_invocation<T>(msg: T, invocation: &str) -> tonic::Request<T> {
    with_metadata(msg, invocation, "", "")
}

/// An endpoint nothing listens on (grab a port, then release it).
async fn dead_endpoint() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

async fn upload(
    cas: &mut CasClient,
    data: &[u8],
    invocation: &str,
) -> reapi::Digest {
    let digest = digest(data);
    let updated = cas
        .batch_update_blobs(with_invocation(
            reapi::BatchUpdateBlobsRequest {
                instance_name: String::new(),
                requests: vec![reapi::batch_update_blobs_request::Request {
                    digest: Some(digest.clone()),
                    data: data.to_vec(),
                    compressor: 0,
                }],
                digest_function: 0,
            },
            invocation,
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(updated.responses.iter().all(|r| r.status.is_none()));
    digest
}

async fn is_present(cas: &mut CasClient, digest: &reapi::Digest) -> bool {
    cas.find_missing_blobs(reapi::FindMissingBlobsRequest {
        instance_name: String::new(),
        blob_digests: vec![digest.clone()],
        digest_function: 0,
    })
    .await
    .unwrap()
    .into_inner()
    .missing_blob_digests
    .is_empty()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routes_new_builds_to_first_healthy_backend() {
    let a = Instance::backend().await;
    let b = Instance::backend().await;
    let mux = Instance::spawn(false, &format!("{}{}", a.remote_toml("a", ""), b.remote_toml("b", ""))).await;

    let mut mux_cas = mux.cas().await;
    let uploaded = upload(&mut mux_cas, b"preferred backend wins", "build-1").await;

    // The blob landed on a, and only on a.
    assert!(is_present(&mut a.cas().await, &uploaded).await);
    assert!(!is_present(&mut b.cas().await, &uploaded).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pinned_builds_fail_loudly_while_new_builds_fail_over() {
    // NOTE: the dead backend is dead from the start rather than killed
    // mid-test: aborting an in-process server only closes its listener,
    // while already-established mux->backend connections keep serving
    // (hyper drives them on independent tasks). A real death (process
    // exit, dead NIC) severs those too; refusing the connection exercises
    // the identical mux path (pin, forward, classify, report, open).
    let dead = dead_endpoint().await;
    let b = Instance::backend().await;
    // Threshold 1 + a huge cooldown: one failure opens the breaker for the
    // whole test, deterministically.
    let mux = Instance::spawn(
        false,
        &format!(
            "[[backends]]\nname = \"a\"\nendpoint = \"{dead}\"\nmax_consecutive_failures = 1\ncooldown_secs = 3600\n{}",
            b.remote_toml("b", "")
        ),
    )
    .await;

    // build-1 pins to the preferred (dead) backend at route time; its RPCs
    // fail with Unavailable naming that backend (never silently moving to
    // b), which is what buck2's restarter recovers from under a fresh
    // invocation id.
    let mut mux_cas = mux.cas().await;
    let data = b"pinned bytes";
    let first = digest(data);
    let err = mux_cas
        .batch_update_blobs(with_invocation(
            reapi::BatchUpdateBlobsRequest {
                instance_name: String::new(),
                requests: vec![reapi::batch_update_blobs_request::Request {
                    digest: Some(first.clone()),
                    data: data.to_vec(),
                    compressor: 0,
                }],
                digest_function: 0,
            },
            "build-1",
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unavailable);
    assert!(err.message().contains("backend a:"), "{}", err.message());

    // A new build routes around the open breaker to b.
    let second = upload(&mut mux_cas, b"on backend b", "build-2").await;
    assert!(is_present(&mut b.cas().await, &second).await);

    // And the old pin still holds: build-1 keeps failing, never migrates.
    let err = mux_cas
        .find_missing_blobs(with_invocation(
            reapi::FindMissingBlobsRequest {
                instance_name: String::new(),
                blob_digests: vec![first],
                digest_function: 0,
            },
            "build-1",
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unavailable);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_backend_serves_when_remotes_are_down() {
    let dead = dead_endpoint().await;
    let mux = Instance::spawn(
        true,
        &format!(
            "[[backends]]\nname = \"gone\"\nendpoint = \"{dead}\"\nmax_consecutive_failures = 1\ncooldown_secs = 3600\n[[backends]]\nname = \"local\"\nendpoint = \"self\"\n"
        ),
    )
    .await;

    let mut mux_cas = mux.cas().await;
    // First build trips the dead remote's breaker.
    let err = mux_cas
        .find_missing_blobs(with_invocation(
            reapi::FindMissingBlobsRequest {
                instance_name: String::new(),
                blob_digests: vec![],
                digest_function: 0,
            },
            "build-1",
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unavailable);

    // New builds fall through to self: uploads land in the mux's own store.
    let uploaded = upload(&mut mux_cas, b"offline bytes", "build-2").await;
    assert!(is_present(&mut mux.cas().await, &uploaded).await);
    // A fresh invocation routes to self too (dead remote still cooling).
    let missing = mux_cas
        .find_missing_blobs(with_invocation(
            reapi::FindMissingBlobsRequest {
                instance_name: String::new(),
                blob_digests: vec![uploaded],
                digest_function: 0,
            },
            "build-3",
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(missing.missing_blob_digests.is_empty());

    // The tripped build stays pinned to the dead remote.
    let err = mux_cas
        .find_missing_blobs(with_invocation(
            reapi::FindMissingBlobsRequest {
                instance_name: String::new(),
                blob_digests: vec![],
                digest_function: 0,
            },
            "build-1",
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unavailable);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_backends_down_returns_unavailable_naming_preferred() {
    let dead1 = dead_endpoint().await;
    let dead2 = dead_endpoint().await;
    let mux = Instance::spawn(
        false,
        &format!(
            "[[backends]]\nname = \"one\"\nendpoint = \"{dead1}\"\n[[backends]]\nname = \"two\"\nendpoint = \"{dead2}\"\n"
        ),
    )
    .await;
    let err = mux
        .cas()
        .await
        .find_missing_blobs(with_invocation(
            reapi::FindMissingBlobsRequest {
                instance_name: String::new(),
                blob_digests: vec![],
                digest_function: 0,
            },
            "build-1",
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unavailable);
    assert!(err.message().contains("backend one:"), "{}", err.message());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capabilities_are_answered_locally_with_backends_down() {
    let dead = dead_endpoint().await;
    let mux = Instance::spawn(
        false,
        &format!("[[backends]]\nname = \"gone\"\nendpoint = \"{dead}\"\n"),
    )
    .await;
    let caps = mux
        .caps()
        .await
        .get_capabilities(reapi::GetCapabilitiesRequest {
            instance_name: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    let cache = caps.cache_capabilities.unwrap();
    assert_eq!(cache.digest_functions, vec![1, 2, 9]);
    assert!(caps.execution_capabilities.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwards_request_metadata_to_backends() {
    let a = Instance::backend().await;
    let mux = Instance::spawn(false, &a.remote_toml("a", "")).await;

    let mut mux_cas = mux.cas().await;
    let data = b"metadata probe";
    let uploaded = digest(data);
    mux_cas
        .batch_update_blobs(with_metadata(
            reapi::BatchUpdateBlobsRequest {
                instance_name: String::new(),
                requests: vec![reapi::batch_update_blobs_request::Request {
                    digest: Some(uploaded.clone()),
                    data: data.to_vec(),
                    compressor: 0,
                }],
                digest_function: 0,
            },
            "build-9",
            "Rustc",
            "//some:target",
        ))
        .await
        .unwrap();

    // The backend stored the mux's client metadata as its own hints.
    let store = Store::open(a.store_dir.clone().unwrap(), 1 << 30, 1_700_000_001).await.unwrap();
    let (_, meta) = store.cas_lookup(&uploaded.hash, 0).unwrap();
    assert_eq!(meta.hints.tool_invocation_id.as_deref(), Some("build-9"));
    assert_eq!(meta.hints.mnemonic.as_deref(), Some("Rustc"));
    assert_eq!(meta.hints.target_id.as_deref(), Some("//some:target"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bytestream_proxies_read_and_write() {
    let a = Instance::backend().await;
    let mux = Instance::spawn(false, &a.remote_toml("a", "")).await;

    let data = vec![9u8; 100_000];
    let uploaded = digest(&data);
    let name = format!("main/blobs/{}/{}", uploaded.hash, uploaded.size_bytes);

    let mut mux_bs = mux.bs().await;
    let resp = mux_bs
        .write(with_invocation(
            tokio_stream::iter(vec![bytestream::WriteRequest {
                resource_name: name.clone(),
                write_offset: 0,
                finish_write: true,
                data: data.clone(),
            }]),
            "build-1",
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.committed_size, data.len() as i64);

    // Read back through the mux.
    let mut stream = mux_bs
        .read(with_invocation(
            bytestream::ReadRequest {
                resource_name: name.clone(),
                read_offset: 0,
                read_limit: 0,
            },
            "build-1",
        ))
        .await
        .unwrap()
        .into_inner();
    let mut got = Vec::new();
    while let Some(chunk) = stream.message().await.unwrap() {
        got.extend_from_slice(&chunk.data);
    }
    assert_eq!(got, data);

    // ... and the bytes live on the backend, proving both directions
    // forwarded rather than serving locally.
    let mut stream = a
        .bs()
        .await
        .read(bytestream::ReadRequest {
            resource_name: name,
            read_offset: 0,
            read_limit: 0,
        })
        .await
        .unwrap()
        .into_inner();
    let mut direct = Vec::new();
    while let Some(chunk) = stream.message().await.unwrap() {
        direct.extend_from_slice(&chunk.data);
    }
    assert_eq!(direct, data);
}
