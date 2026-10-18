//! End-to-end gRPC round trips against an in-process `reapid`.
//!
//! Each test spins a server on an ephemeral loopback port backed by a fresh
//! tempdir store, then drives it with the generated tonic clients. This is
//! the closest honest proxy to a real buck2 client short of E2E (see
//! `remote_execution/reapid/e2e.sh` for the buck2-driven pass).

use std::sync::Arc;

use reapi_cache::config::Config;
use reapi_cache::digest::DigestFunction;
use reapi_cache::services::Shared;
use reapi_cache::store::Store;

struct Ctx {
    _temp: tempfile::TempDir,
    store_dir: std::path::PathBuf,
    cas: reapi::content_addressable_storage_client::ContentAddressableStorageClient<tonic::transport::Channel>,
    ac: reapi::action_cache_client::ActionCacheClient<tonic::transport::Channel>,
    caps: reapi::capabilities_client::CapabilitiesClient<tonic::transport::Channel>,
    bs: bytestream::byte_stream_client::ByteStreamClient<tonic::transport::Channel>,
}

impl Ctx {
    async fn new(toml_extra: &str) -> Ctx {
        let temp = tempfile::TempDir::new().unwrap();
        let store_dir = temp.path().join("store");
        let config = Config::from_toml(&format!(
            "bind = \"127.0.0.1:0\"\nstore_dir = \"{}\"\n{toml_extra}\n[[backends]]\nname = \"local\"\nendpoint = \"self\"\n",
            store_dir.display()
        ))
        .unwrap();
        // Bind 127.0.0.1:0 first so the OS picks a free port (avoids parallel
        // test conflicts); serve() takes the bound listener.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let store = Store::open(store_dir.clone(), config.max_blob_bytes, 1_700_000_000)
            .await
            .unwrap();
        let mux =
            reapi_cache::mux::Mux::new(&config.backends, reapi_cache::services::frame_bytes(config.max_batch_bytes))
                .unwrap();
        let shared = Arc::new(Shared::new(Some(store), config, mux));
        tokio::spawn(reapi_cache::services::serve(listener, shared));
        let endpoint = format!("http://{addr}");
        Ctx {
            _temp: temp,
            store_dir,
            cas: reapi::content_addressable_storage_client::ContentAddressableStorageClient::connect(
                endpoint.clone(),
            )
            .await
            .unwrap(),
            ac: reapi::action_cache_client::ActionCacheClient::connect(endpoint.clone())
                .await
                .unwrap(),
            caps: reapi::capabilities_client::CapabilitiesClient::connect(endpoint.clone())
                .await
                .unwrap(),
            bs: bytestream::byte_stream_client::ByteStreamClient::connect(endpoint).await.unwrap(),
        }
    }

    fn digest(data: &[u8]) -> reapi::Digest {
        reapi::Digest {
            hash: DigestFunction::Sha256.hexdigest(data),
            size_bytes: data.len() as i64,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capabilities_advertise_cache_only() {
    let mut ctx = Ctx::new("").await;
    let caps = ctx
        .caps
        .get_capabilities(reapi::GetCapabilitiesRequest {
            instance_name: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    let cache = caps.cache_capabilities.unwrap();
    assert_eq!(cache.digest_functions, vec![1, 2, 9]);
    assert!(cache.action_cache_update_capabilities.unwrap().update_enabled);
    assert_eq!(cache.max_batch_total_size_bytes, 4 << 20);
    assert_eq!(cache.supported_compressors, vec![1]);
    assert!(caps.execution_capabilities.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cas_batch_round_trip() {
    let mut ctx = Ctx::new("").await;
    let data = b"batch blob bytes".to_vec();
    let digest = Ctx::digest(&data);

    // Missing before upload.
    let missing = ctx
        .cas
        .find_missing_blobs(reapi::FindMissingBlobsRequest {
            instance_name: "main".to_string(),
            blob_digests: vec![digest.clone()],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(missing.missing_blob_digests, vec![digest.clone()]);

    // Upload (identity + a zstd-compressed twin).
    let zst_data = vec![42u8; 50_000];
    let zst_digest = Ctx::digest(&zst_data);
    let zst = ruzstd::encoding::compress_to_vec(&zst_data[..], ruzstd::encoding::CompressionLevel::Fastest);
    let updated = ctx
        .cas
        .batch_update_blobs(reapi::BatchUpdateBlobsRequest {
            instance_name: String::new(),
            requests: vec![
                reapi::batch_update_blobs_request::Request {
                    digest: Some(digest.clone()),
                    data: data.clone(),
                    compressor: 0,
                },
                reapi::batch_update_blobs_request::Request {
                    digest: Some(zst_digest.clone()),
                    data: zst,
                    compressor: 1,
                },
            ],
            digest_function: 1,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(updated.responses.len(), 2);
    assert!(updated.responses.iter().all(|r| r.status.is_none()));

    // Present after upload (explicit and inferred function agree).
    for function in [0, 1] {
        let missing = ctx
            .cas
            .find_missing_blobs(reapi::FindMissingBlobsRequest {
                instance_name: String::new(),
                blob_digests: vec![digest.clone(), zst_digest.clone()],
                digest_function: function,
            })
            .await
            .unwrap()
            .into_inner();
        assert!(missing.missing_blob_digests.is_empty());
    }

    // Read back identity.
    let read = ctx
        .cas
        .batch_read_blobs(reapi::BatchReadBlobsRequest {
            instance_name: String::new(),
            digests: vec![digest.clone()],
            acceptable_compressors: vec![],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(read.responses.len(), 1);
    assert_eq!(read.responses[0].data, data);
    assert_eq!(read.responses[0].compressor, 0);
    assert!(read.responses[0].status.is_none());

    // Read back zstd (client accepts only zstd).
    let read = ctx
        .cas
        .batch_read_blobs(reapi::BatchReadBlobsRequest {
            instance_name: String::new(),
            digests: vec![zst_digest.clone()],
            acceptable_compressors: vec![1],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(read.responses[0].compressor, 1);
    let mut decoder = ruzstd::decoding::FrameDecoder::new();
    let mut out = vec![0u8; zst_data.len()];
    let n = decoder.decode_all(&read.responses[0].data, &mut out).unwrap();
    assert_eq!(&out[..n], &zst_data[..]);

    // Missing blob: per-response NOT_FOUND, RPC ok.
    let other = Ctx::digest(b"never uploaded");
    let read = ctx
        .cas
        .batch_read_blobs(reapi::BatchReadBlobsRequest {
            instance_name: String::new(),
            digests: vec![other],
            acceptable_compressors: vec![],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(read.responses[0].status.as_ref().unwrap().code, tonic::Code::NotFound as i32);

    // Corrupt upload: per-response INVALID_ARGUMENT, nothing stored.
    let bad = ctx
        .cas
        .batch_update_blobs(reapi::BatchUpdateBlobsRequest {
            instance_name: String::new(),
            requests: vec![reapi::batch_update_blobs_request::Request {
                digest: Some(digest.clone()),
                data: b"wrong bytes!!".to_vec(),
                compressor: 0,
            }],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        bad.responses[0].status.as_ref().unwrap().code,
        tonic::Code::InvalidArgument as i32
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bytestream_write_resume_and_read() {
    let mut ctx = Ctx::new("").await;
    let data = vec![7u8; 200_000];
    let digest = Ctx::digest(&data);
    let name = format!("main/uploads/resume-1/blobs/{}/{}", digest.hash, digest.size_bytes);

    // First leg: two chunks, stream closes without finish.
    let leg1 = vec![
        bytestream::WriteRequest {
            resource_name: name.clone(),
            write_offset: 0,
            finish_write: false,
            data: data[..70_000].to_vec(),
        },
        bytestream::WriteRequest {
            resource_name: String::new(),
            write_offset: 70_000,
            finish_write: false,
            data: data[70_000..120_000].to_vec(),
        },
    ];
    let resp = ctx
        .bs
        .write(tonic::Request::new(tokio_stream::iter(leg1)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.committed_size, 120_000);

    // Status reports the committed prefix.
    let status = ctx
        .bs
        .query_write_status(bytestream::QueryWriteStatusRequest { resource_name: name.clone() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.committed_size, 120_000);
    assert!(!status.complete);

    // Resume at the committed offset and finish.
    let leg2 = vec![bytestream::WriteRequest {
        resource_name: name.clone(),
        write_offset: 120_000,
        finish_write: true,
        data: data[120_000..].to_vec(),
    }];
    let resp = ctx
        .bs
        .write(tonic::Request::new(tokio_stream::iter(leg2)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(resp.committed_size, data.len() as i64);

    // Status after finish: complete (the bytes made it to CAS).
    let status = ctx
        .bs
        .query_write_status(bytestream::QueryWriteStatusRequest { resource_name: name.clone() })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(status.committed_size, data.len() as i64);
    assert!(status.complete);

    // Full read back.
    let read_name = format!("main/blobs/{}/{}", digest.hash, digest.size_bytes);
    let mut stream = ctx
        .bs
        .read(bytestream::ReadRequest {
            resource_name: read_name.clone(),
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
    assert_eq!(got, data);

    // Offset + limit read.
    let mut stream = ctx
        .bs
        .read(bytestream::ReadRequest {
            resource_name: read_name,
            read_offset: 100,
            read_limit: 1000,
        })
        .await
        .unwrap()
        .into_inner();
    let mut got = Vec::new();
    while let Some(chunk) = stream.message().await.unwrap() {
        got.extend_from_slice(&chunk.data);
    }
    assert_eq!(got, data[100..1100]);

    // Offset past the end is OUT_OF_RANGE.
    let err = ctx
        .bs
        .read(bytestream::ReadRequest {
            resource_name: format!("blobs/{}/{}", digest.hash, digest.size_bytes),
            read_offset: data.len() as i64 + 1,
            read_limit: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::OutOfRange);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bytestream_compressed_and_empty() {
    let mut ctx = Ctx::new("").await;
    let data = vec![3u8; 80_000];
    let digest = Ctx::digest(&data);
    let zst = ruzstd::encoding::compress_to_vec(&data[..], ruzstd::encoding::CompressionLevel::Fastest);

    // Compressed upload through ByteStream.
    let up_name = format!("uploads/z1/compressed-blobs/zstd/{}/{}", digest.hash, digest.size_bytes);
    let resp = ctx
        .bs
        .write(tonic::Request::new(tokio_stream::iter(vec![bytestream::WriteRequest {
            resource_name: up_name,
            write_offset: 0,
            finish_write: true,
            data: zst.clone(),
        }])))
        .await
        .unwrap()
        .into_inner();
    // committed_size echoes wire bytes, not the decoded size (buck2 rejects
    // anything else with "invalid committed_size").
    assert_eq!(resp.committed_size, zst.len() as i64);

    // Both forms read back; the compressed form is the sender's exact bytes.
    for (suffix, want) in [
        (format!("blobs/{}/{}", digest.hash, digest.size_bytes), data.clone()),
        (
            format!("compressed-blobs/zstd/{}/{}", digest.hash, digest.size_bytes),
            zst.clone(),
        ),
    ] {
        let mut stream = ctx
            .bs
            .read(bytestream::ReadRequest {
                resource_name: suffix,
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
        assert_eq!(got, want);
    }

    // The empty blob reads as an empty stream without ever being uploaded.
    let empty = DigestFunction::Sha256.empty_hex();
    let mut stream = ctx
        .bs
        .read(bytestream::ReadRequest {
            resource_name: format!("blobs/{empty}/0"),
            read_offset: 0,
            read_limit: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(stream.message().await.unwrap().is_none());

    // Garbage resource names fail fast.
    let err = ctx
        .bs
        .read(bytestream::ReadRequest {
            resource_name: "blobs/zz/3".to_string(),
            read_offset: 0,
            read_limit: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    // Unknown upload status is NOT_FOUND (well-formed name, unknown id).
    let err = ctx
        .bs
        .query_write_status(bytestream::QueryWriteStatusRequest {
            resource_name: format!("uploads/nope/blobs/{}/1", DigestFunction::Sha256.hexdigest(b"x")),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compressed_empty_blob_reads() {
    // The empty blob synthesizes without storage; compressed reads of it
    // must serve the zstd encoding of empty (never NOT_FOUND).
    let mut ctx = Ctx::new("").await;
    let empty = DigestFunction::Sha256.empty_hex();

    let read = ctx
        .cas
        .batch_read_blobs(reapi::BatchReadBlobsRequest {
            instance_name: String::new(),
            digests: vec![reapi::Digest {
                hash: empty.clone(),
                size_bytes: 0,
            }],
            acceptable_compressors: vec![1],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(read.responses[0].compressor, 1);
    assert!(read.responses[0].status.is_none());
    let mut decoder = ruzstd::decoding::FrameDecoder::new();
    let mut out = vec![0u8; 16];
    let n = decoder.decode_all(&read.responses[0].data, &mut out).unwrap();
    assert_eq!(n, 0);
    assert!(!read.responses[0].data.is_empty());

    let mut stream = ctx
        .bs
        .read(bytestream::ReadRequest {
            resource_name: format!("compressed-blobs/zstd/{empty}/0"),
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
    let mut decoder = ruzstd::decoding::FrameDecoder::new();
    let mut out = vec![0u8; 16];
    assert_eq!(decoder.decode_all(&got, &mut out).unwrap(), 0);
    assert!(!got.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_cache_and_tree() {
    let mut ctx = Ctx::new("").await;

    // ActionCache miss then hit.
    let action = Ctx::digest(b"fake action");
    let err = ctx
        .ac
        .get_action_result(reapi::GetActionResultRequest {
            instance_name: String::new(),
            action_digest: Some(action.clone()),
            inline_stdout: false,
            inline_stderr: false,
            inline_output_files: vec![],
            digest_function: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);

    let output = Ctx::digest(b"fake output");
    // Upload the output blob first (as a well-behaved client would).
    ctx.cas
        .batch_update_blobs(reapi::BatchUpdateBlobsRequest {
            instance_name: String::new(),
            requests: vec![reapi::batch_update_blobs_request::Request {
                digest: Some(output.clone()),
                data: b"fake output".to_vec(),
                compressor: 0,
            }],
            digest_function: 0,
        })
        .await
        .unwrap();
    let result = reapi::ActionResult {
        output_files: vec![reapi::OutputFile {
            path: "out.bin".to_string(),
            digest: Some(output.clone()),
            is_executable: false,
            contents: vec![],
            node_properties: None,
        }],
        exit_code: 0,
        ..Default::default()
    };
    let echoed = ctx
        .ac
        .update_action_result(reapi::UpdateActionResultRequest {
            instance_name: String::new(),
            action_digest: Some(action.clone()),
            action_result: Some(result.clone()),
            results_cache_policy: None,
            digest_function: 1,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(echoed.exit_code, 0);
    let hit = ctx
        .ac
        .get_action_result(reapi::GetActionResultRequest {
            instance_name: String::new(),
            action_digest: Some(action.clone()),
            inline_stdout: false,
            inline_stderr: false,
            inline_output_files: vec![],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(hit.output_files.len(), 1);
    assert_eq!(hit.output_files[0].digest, Some(output.clone()));

    // GetTree over a two-level directory.
    let file_digest = Ctx::digest(b"file bytes");
    ctx.cas
        .batch_update_blobs(reapi::BatchUpdateBlobsRequest {
            instance_name: String::new(),
            requests: vec![reapi::batch_update_blobs_request::Request {
                digest: Some(file_digest.clone()),
                data: b"file bytes".to_vec(),
                compressor: 0,
            }],
            digest_function: 0,
        })
        .await
        .unwrap();
    let leaf = reapi::Directory {
        files: vec![reapi::FileNode {
            name: "f.txt".to_string(),
            digest: Some(file_digest.clone()),
            is_executable: false,
            node_properties: None,
        }],
        directories: vec![],
        symlinks: vec![],
        node_properties: None,
        ..Default::default()
    };
    let mut leaf_bytes = Vec::new();
    prost::Message::encode(&leaf, &mut leaf_bytes).unwrap();
    let leaf_digest = Ctx::digest(&leaf_bytes);
    let root = reapi::Directory {
        files: vec![],
        directories: vec![reapi::DirectoryNode {
            name: "sub".to_string(),
            digest: Some(leaf_digest.clone()),
        }],
        symlinks: vec![],
        node_properties: None,
        ..Default::default()
    };
    let mut root_bytes = Vec::new();
    prost::Message::encode(&root, &mut root_bytes).unwrap();
    let root_digest = Ctx::digest(&root_bytes);
    ctx.cas
        .batch_update_blobs(reapi::BatchUpdateBlobsRequest {
            instance_name: String::new(),
            requests: vec![
                reapi::batch_update_blobs_request::Request {
                    digest: Some(leaf_digest.clone()),
                    data: leaf_bytes,
                    compressor: 0,
                },
                reapi::batch_update_blobs_request::Request {
                    digest: Some(root_digest.clone()),
                    data: root_bytes,
                    compressor: 0,
                },
            ],
            digest_function: 0,
        })
        .await
        .unwrap();
    let mut stream = ctx
        .cas
        .get_tree(reapi::GetTreeRequest {
            instance_name: String::new(),
            root_digest: Some(root_digest.clone()),
            page_size: 0,
            page_token: String::new(),
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    let mut dirs = Vec::new();
    while let Some(resp) = stream.message().await.unwrap() {
        assert!(resp.next_page_token.is_empty());
        dirs.extend(resp.directories);
    }
    assert_eq!(dirs.len(), 2);

    // Missing tree root is NOT_FOUND.
    let err = ctx
        .cas
        .get_tree(reapi::GetTreeRequest {
            instance_name: String::new(),
            root_digest: Some(Ctx::digest(b"nope")),
            page_size: 0,
            page_token: String::new(),
            digest_function: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_metadata_lands_in_sidecars() {
    let mut ctx = Ctx::new("").await;
    let data = b"hinted bytes".to_vec();
    let digest = Ctx::digest(&data);

    let meta = reapi::RequestMetadata {
        tool_invocation_id: "inv-123".to_string(),
        action_mnemonic: "Rustc".to_string(),
        target_id: "//foo:bar".to_string(),
        configuration_id: "cfg-x".to_string(),
        ..Default::default()
    };
    let mut bytes = Vec::new();
    prost::Message::encode(&meta, &mut bytes).unwrap();
    let mut req = tonic::Request::new(reapi::BatchUpdateBlobsRequest {
        instance_name: String::new(),
        requests: vec![reapi::batch_update_blobs_request::Request {
            digest: Some(digest.clone()),
            data,
            compressor: 0,
        }],
        digest_function: 0,
    });
    req.metadata_mut().insert_bin(
        "build.bazel.remote.execution.v2.requestmetadata-bin",
        tonic::metadata::MetadataValue::from_bytes(&bytes),
    );
    ctx.cas.batch_update_blobs(req).await.unwrap();

    // The sidecar on disk carries the captured hints (real gRPC transport,
    // including base64 header encoding, in between).
    let sidecar = ctx
        .store_dir
        .join("cas/sha256")
        .join(&digest.hash[..2])
        .join(format!("{}.meta.json", digest.hash));
    let text = std::fs::read_to_string(&sidecar).unwrap();
    for want in ["\"mnemonic\": \"Rustc\"", "\"target_id\": \"//foo:bar\"", "\"tool_invocation_id\": \"inv-123\""] {
        assert!(text.contains(want), "sidecar lacks {want}: {text}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_evicts_lru_over_grpc() {
    // 100-byte quota with no rules: plain LRU across uploads.
    let mut ctx = Ctx::new("max_store_bytes = 100").await;
    let mut digests = Vec::new();
    for i in 0..5u8 {
        let data = vec![i; 30];
        let digest = Ctx::digest(&data);
        ctx.cas
            .batch_update_blobs(reapi::BatchUpdateBlobsRequest {
                instance_name: String::new(),
                requests: vec![reapi::batch_update_blobs_request::Request {
                    digest: Some(digest.clone()),
                    data,
                    compressor: 0,
                }],
                digest_function: 0,
            })
            .await
            .unwrap();
        digests.push(digest);
    }
    // 5 x 30 = 150 > 100: the two oldest are gone, the rest survive.
    let missing = ctx
        .cas
        .find_missing_blobs(reapi::FindMissingBlobsRequest {
            instance_name: String::new(),
            blob_digests: digests.clone(),
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(missing.missing_blob_digests, digests[..2].to_vec());
}
