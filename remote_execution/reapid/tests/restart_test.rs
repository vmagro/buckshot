//! Restart durability: a `reapid` power cycle loses nothing acknowledged.
//!
//! The deploy target reboots frequently onto a persistent disk, so the
//! store must reopen with everything it had: blobs in every digest
//! function and both compressed forms, ActionCache entries, hint metadata,
//! and access stats. Each test populates a server over real gRPC, kills it
//! (aborting the serve task drops everything in memory, like power loss),
//! reopens the same store dir, and re-verifies. Unfinished ByteStream
//! uploads are the deliberate exception: they never resume across
//! restarts, and must read back as missing rather than corrupt.

use std::sync::Arc;

use reapi_cache::config::Config;
use reapi_cache::digest::DigestFunction;
use reapi_cache::services::Shared;
use reapi_cache::store::Store;

type CasClient = reapi::content_addressable_storage_client::ContentAddressableStorageClient<tonic::transport::Channel>;
type AcClient = reapi::action_cache_client::ActionCacheClient<tonic::transport::Channel>;
type BsClient = bytestream::byte_stream_client::ByteStreamClient<tonic::transport::Channel>;

const METADATA_HEADER: &str = "build.bazel.remote.execution.v2.requestmetadata-bin";

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

/// One daemon generation: fresh process-equivalent state (new Store, new
/// listener) over the same persistent store dir.
struct Generation {
    cas: CasClient,
    ac: AcClient,
    bs: BsClient,
    shared: Arc<Shared>,
    handle: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}

impl Generation {
    /// Power cut: abort the serve task (dropping the handle would only
    /// detach it and leave the old generation serving).
    fn kill(self) {
        self.handle.abort();
    }
}

struct Daemon {
    _temp: tempfile::TempDir,
    store_dir: std::path::PathBuf,
}

impl Daemon {
    fn new() -> Daemon {
        let temp = tempfile::TempDir::new().unwrap();
        let store_dir = temp.path().join("store");
        Daemon {
            _temp: temp,
            store_dir,
        }
    }

    /// Boot (or reboot): open the store and serve it on a fresh port. The
    /// previous generation's handle is dropped by the caller, which aborts
    /// its serve task — in-memory index, uploads, and connections all gone,
    /// exactly like a power cycle.
    async fn boot(&self) -> Generation {
        let config = Config::from_toml(&format!(
            "bind = \"127.0.0.1:0\"\nstore_dir = \"{}\"\n[[backends]]\nname = \"local\"\nendpoint = \"self\"\n",
            self.store_dir.display()
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let store = Store::open(self.store_dir.clone(), config.max_blob_bytes, 1_700_000_000)
            .await
            .unwrap();
        let mux = reapi_cache::mux::Mux::new(
            &config.backends,
            reapi_cache::services::frame_bytes(config.max_batch_bytes),
        )
        .unwrap();
        let shared = Arc::new(Shared::new(Some(store), config, mux));
        let handle = tokio::spawn(reapi_cache::services::serve(listener, shared.clone()));
        Generation {
            cas: CasClient::connect(endpoint.clone()).await.unwrap(),
            ac: AcClient::connect(endpoint.clone()).await.unwrap(),
            bs: BsClient::connect(endpoint).await.unwrap(),
            shared,
            handle,
        }
    }
}

fn sha256(data: &[u8]) -> reapi::Digest {
    reapi::Digest {
        hash: DigestFunction::Sha256.hexdigest(data),
        size_bytes: data.len() as i64,
    }
}

async fn read_all(bs: &mut BsClient, resource_name: String) -> Vec<u8> {
    let mut stream = bs
        .read(bytestream::ReadRequest {
            resource_name,
            read_offset: 0,
            read_limit: 0,
        })
        .await
        .unwrap()
        .into_inner();
    let mut got = Vec::new();
    while let Some(chunk) = stream.message().await.unwrap() {
        got.extend_from_slice(&chunk.data);
    }
    got
}

/// Everything the restart test stores, for re-verification after each boot.
struct Fixture {
    plain: (reapi::Digest, Vec<u8>),
    sha1: (reapi::Digest, Vec<u8>),
    blake3: (reapi::Digest, Vec<u8>),
    zstd: (reapi::Digest, Vec<u8>, Vec<u8>),
    streamed: (reapi::Digest, Vec<u8>),
    unfinished: reapi::Digest,
    unfinished_upload: String,
    action: reapi::Digest,
    result: reapi::ActionResult,
}

async fn populate(d: &mut Generation) -> Fixture {
    let plain_data = b"restart me".to_vec();
    let plain = sha256(&plain_data);
    let sha1_data = b"sha1 survives".to_vec();
    let sha1 = reapi::Digest {
        hash: DigestFunction::Sha1.hexdigest(&sha1_data),
        size_bytes: sha1_data.len() as i64,
    };
    let blake3_data = b"blake3 survives".to_vec();
    let blake3 = reapi::Digest {
        hash: DigestFunction::Blake3.hexdigest(&blake3_data),
        size_bytes: blake3_data.len() as i64,
    };
    let zst_data = vec![11u8; 60_000];
    let zst_digest = sha256(&zst_data);
    let zst = ruzstd::encoding::compress_to_vec(&zst_data[..], ruzstd::encoding::CompressionLevel::Fastest);
    let streamed_data = vec![13u8; 90_000];
    let streamed = sha256(&streamed_data);
    let unfinished_data = b"cut off mid-upload".to_vec();
    let unfinished = sha256(&unfinished_data);

    // Batch uploads: identity (explicit functions) + one zstd twin.
    for (digest, data, compressor, function) in [
        (&plain, plain_data.clone(), 0, DigestFunction::Sha256.to_proto()),
        (&sha1, sha1_data.clone(), 0, DigestFunction::Sha1.to_proto()),
        (&blake3, blake3_data.clone(), 0, DigestFunction::Blake3.to_proto()),
    ] {
        let updated = d
            .cas
            .batch_update_blobs(with_metadata(
                reapi::BatchUpdateBlobsRequest {
                    instance_name: String::new(),
                    requests: vec![reapi::batch_update_blobs_request::Request {
                        digest: Some(digest.clone()),
                        data,
                        compressor,
                    }],
                    digest_function: function,
                },
                "build-1",
                "Rustc",
                "//restart:target",
            ))
            .await
            .unwrap()
            .into_inner();
        assert!(updated.responses.iter().all(|r| r.status.is_none()));
    }
    let updated = d
        .cas
        .batch_update_blobs(with_metadata(
            reapi::BatchUpdateBlobsRequest {
                instance_name: String::new(),
                requests: vec![reapi::batch_update_blobs_request::Request {
                    digest: Some(zst_digest.clone()),
                    data: zst.clone(),
                    compressor: 1,
                }],
                digest_function: 0,
            },
            "build-1",
            "Rustc",
            "//restart:target",
        ))
        .await
        .unwrap()
        .into_inner();
    assert!(updated.responses.iter().all(|r| r.status.is_none()));

    // A finished multi-chunk ByteStream upload.
    let chunks = vec![
        bytestream::WriteRequest {
            resource_name: format!("main/uploads/fin-1/blobs/{}/{}", streamed.hash, streamed.size_bytes),
            write_offset: 0,
            finish_write: false,
            data: streamed_data[..30_000].to_vec(),
        },
        bytestream::WriteRequest {
            resource_name: String::new(),
            write_offset: 30_000,
            finish_write: true,
            data: streamed_data[30_000..].to_vec(),
        },
    ];
    let resp = d
        .bs
        .write(with_metadata(tokio_stream::iter(chunks), "build-1", "Genrule", "//restart:stream"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.committed_size, streamed_data.len() as i64);

    // An UNFINISHED upload: chunks committed, stream ends without finish.
    let unfinished_upload =
        format!("main/uploads/cut-1/blobs/{}/{}", unfinished.hash, unfinished.size_bytes);
    let resp = d
        .bs
        .write(with_metadata(
            tokio_stream::iter(vec![bytestream::WriteRequest {
                resource_name: unfinished_upload.clone(),
                write_offset: 0,
                finish_write: false,
                data: unfinished_data[..8].to_vec(),
            }]),
            "build-1",
            "",
            "",
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.committed_size, 8);

    // An ActionCache entry (output blob uploaded first, as clients do).
    let output = sha256(b"restart output");
    d.cas
        .batch_update_blobs(with_metadata(
            reapi::BatchUpdateBlobsRequest {
                instance_name: String::new(),
                requests: vec![reapi::batch_update_blobs_request::Request {
                    digest: Some(output.clone()),
                    data: b"restart output".to_vec(),
                    compressor: 0,
                }],
                digest_function: 0,
            },
            "build-1",
            "",
            "",
        ))
        .await
        .unwrap();
    let action = sha256(b"restart action");
    let result = reapi::ActionResult {
        output_files: vec![reapi::OutputFile {
            path: "out.bin".to_string(),
            digest: Some(output),
            is_executable: false,
            contents: vec![],
            node_properties: None,
        }],
        exit_code: 0,
        ..Default::default()
    };
    d.ac
        .update_action_result(with_metadata(
            reapi::UpdateActionResultRequest {
                instance_name: String::new(),
                action_digest: Some(action.clone()),
                action_result: Some(result.clone()),
                results_cache_policy: None,
                digest_function: 0,
            },
            "build-1",
            "Genrule",
            "//restart:action",
        ))
        .await
        .unwrap();

    Fixture {
        plain: (plain, plain_data),
        sha1: (sha1, sha1_data),
        blake3: (blake3, blake3_data),
        zstd: (zst_digest, zst_data, zst),
        streamed: (streamed, streamed_data),
        unfinished,
        unfinished_upload,
        action,
        result,
    }
}

/// Re-verify every fixture fact against a (possibly rebooted) generation.
/// `expect_len` is the store entry count measured before the reboot.
/// `restarted` selects the unfinished-upload expectation: present before
/// the reboot, wiped after.
async fn verify(d: &mut Generation, fix: &Fixture, expect_len: usize, restarted: bool) {
    assert_eq!(d.shared.store.as_ref().unwrap().len(), expect_len);

    // Everything finished is present; the unfinished upload is missing.
    let mut digests = vec![
        fix.plain.0.clone(),
        fix.sha1.0.clone(),
        fix.blake3.0.clone(),
        fix.zstd.0.clone(),
        fix.streamed.0.clone(),
    ];
    let missing = d
        .cas
        .find_missing_blobs(reapi::FindMissingBlobsRequest {
            instance_name: String::new(),
            blob_digests: digests.clone(),
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(missing.missing_blob_digests.is_empty());
    digests.push(fix.unfinished.clone());
    let missing = d
        .cas
        .find_missing_blobs(reapi::FindMissingBlobsRequest {
            instance_name: String::new(),
            blob_digests: digests,
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(missing.missing_blob_digests, vec![fix.unfinished.clone()]);

    // Identity reads back byte-identical.
    for (digest, want) in [fix.plain.clone(), fix.sha1.clone(), fix.blake3.clone(), fix.streamed.clone()] {
        let read = d
            .cas
            .batch_read_blobs(reapi::BatchReadBlobsRequest {
                instance_name: String::new(),
                digests: vec![digest],
                acceptable_compressors: vec![],
                digest_function: 0,
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(read.responses[0].data, want);
        assert!(read.responses[0].status.is_none());
    }

    // The stored zstd form is the sender's exact bytes.
    let read = d
        .cas
        .batch_read_blobs(reapi::BatchReadBlobsRequest {
            instance_name: String::new(),
            digests: vec![fix.zstd.0.clone()],
            acceptable_compressors: vec![1],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(read.responses[0].compressor, 1);
    assert_eq!(read.responses[0].data, fix.zstd.2);

    // ByteStream reads (plain + streamed upload) survive.
    for (digest, want) in [fix.plain.clone(), fix.streamed.clone()] {
        let got = read_all(&mut d.bs, format!("main/blobs/{}/{}", digest.hash, digest.size_bytes)).await;
        assert_eq!(got, want);
    }

    // The unfinished upload: present pre-restart, wiped by the reboot
    // (reads as missing, never corrupt: NOT_FOUND, no blob materialized).
    if restarted {
        let err = d
            .bs
            .query_write_status(bytestream::QueryWriteStatusRequest {
                resource_name: fix.unfinished_upload.clone(),
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    } else {
        let status = d
            .bs
            .query_write_status(bytestream::QueryWriteStatusRequest {
                resource_name: fix.unfinished_upload.clone(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(status.committed_size, 8);
        assert!(!status.complete);
    }

    // The ActionCache entry round-trips.
    let hit = d
        .ac
        .get_action_result(reapi::GetActionResultRequest {
            instance_name: String::new(),
            action_digest: Some(fix.action.clone()),
            inline_stdout: false,
            inline_stderr: false,
            inline_output_files: vec![],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(hit.output_files.len(), 1);
    assert_eq!(hit.output_files[0].digest, fix.result.output_files[0].digest);

    // The empty blob still synthesizes without storage.
    let empty = DigestFunction::Sha256.empty_hex();
    let got = read_all(&mut d.bs, format!("blobs/{empty}/0")).await;
    assert!(got.is_empty());

    // Unknown digests stay missing: the scan invents nothing.
    let ghost = sha256(b"never uploaded");
    let missing = d
        .cas
        .find_missing_blobs(reapi::FindMissingBlobsRequest {
            instance_name: String::new(),
            blob_digests: vec![ghost],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(missing.missing_blob_digests.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_restart_loses_nothing_acknowledged() {
    let daemon = Daemon::new();
    let mut d = daemon.boot().await;
    let fix = populate(&mut d).await;
    let len = d.shared.store.as_ref().unwrap().len();

    // Sanity: everything reads back pre-restart.
    verify(&mut d, &fix, len, false).await;

    // Power cycle (twice: the scan must be idempotent, and repairs from
    // the first reopen must persist verbatim through the second).
    for _ in 0..2 {
        d.kill();
        d = daemon.boot().await;
        verify(&mut d, &fix, len, true).await;
    }

    // Hint metadata survived for policy matching (checked straight from
    // the reopened store).
    let store = Store::open(daemon.store_dir.clone(), 1 << 30, 1_700_000_001).await.unwrap();
    let (_, meta) = store.cas_lookup(&fix.plain.0.hash, 0).unwrap();
    assert_eq!(meta.hints.tool_invocation_id.as_deref(), Some("build-1"));
    assert_eq!(meta.hints.mnemonic.as_deref(), Some("Rustc"));
    assert_eq!(meta.hints.target_id.as_deref(), Some("//restart:target"));
    let (_, meta) = store.cas_lookup(&fix.streamed.0.hash, 0).unwrap();
    assert_eq!(meta.hints.mnemonic.as_deref(), Some("Genrule"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eviction_decisions_survive_restart() {
    // Fill a tiny quota, reboot, and confirm the post-restart evictor sees
    // the same LRU order: the oldest entry goes, not a random one.
    let temp = tempfile::TempDir::new().unwrap();
    let store_dir = temp.path().join("store");
    let config = |extra: &str| {
        Config::from_toml(&format!(
            "bind = \"127.0.0.1:0\"\nstore_dir = \"{}\"\n{extra}\n[[backends]]\nname = \"local\"\nendpoint = \"self\"\n",
            store_dir.display()
        ))
        .unwrap()
    };
    let boot = || async {
        let config = config("max_store_bytes = 100");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let store = Store::open(store_dir.clone(), config.max_blob_bytes, 1_700_000_000).await.unwrap();
        let mux = reapi_cache::mux::Mux::new(
            &config.backends,
            reapi_cache::services::frame_bytes(config.max_batch_bytes),
        )
        .unwrap();
        let shared = Arc::new(Shared::new(Some(store), config, mux));
        let handle = tokio::spawn(reapi_cache::services::serve(listener, shared));
        (CasClient::connect(endpoint).await.unwrap(), handle)
    };

    let blob = |n: u8| vec![n; 30];
    let d0 = sha256(&blob(0));
    let d1 = sha256(&blob(1));
    let d2 = sha256(&blob(2));
    let upload = async |cas: &mut CasClient, digest: &reapi::Digest, data: Vec<u8>| {
        cas.batch_update_blobs(reapi::BatchUpdateBlobsRequest {
            instance_name: String::new(),
            requests: vec![reapi::batch_update_blobs_request::Request {
                digest: Some(digest.clone()),
                data,
                compressor: 0,
            }],
            digest_function: 0,
        })
        .await
        .unwrap()
    };

    let (mut cas, handle) = boot().await;
    // 90 of 100 bytes used; d0 is LRU.
    upload(&mut cas, &d0, blob(0)).await;
    upload(&mut cas, &d1, blob(1)).await;
    upload(&mut cas, &d2, blob(2)).await;
    handle.abort();
    drop(cas);

    // Reboot, then push 30 more bytes: d0 (LRU across the restart) must go.
    let (mut cas, _handle) = boot().await;
    let d3 = sha256(&blob(3));
    upload(&mut cas, &d3, blob(3)).await;
    let missing = cas
        .find_missing_blobs(reapi::FindMissingBlobsRequest {
            instance_name: String::new(),
            blob_digests: vec![d0.clone(), d1.clone(), d2.clone(), d3.clone()],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(missing.missing_blob_digests, vec![d0]);
}
