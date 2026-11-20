//! Execute against an in-process `reapid`: action upload, host-side
//! execution, action-cache hits, and platform enforcement.
//!
//! Commands run through `/bin/sh`, so this whole file is Unix-only. That
//! matches where the daemon runs today (Linux CI, macOS dev).
//!
//! The tests drive both REAPI field spellings (modern `output_paths` and
//! the deprecated `output_files`), so deprecated-field lints are off here.

#![cfg(unix)]
#![allow(deprecated)]

use std::sync::Arc;

use prost::Message;

use reapi_cache::config::Config;
use reapi_cache::digest::DigestFunction;
use reapi_cache::services::Shared;
use reapi_cache::store::Store;

struct Ctx {
    _temp: tempfile::TempDir,
    cas: reapi::content_addressable_storage_client::ContentAddressableStorageClient<
        tonic::transport::Channel,
    >,
    ex: reapi::execution_client::ExecutionClient<tonic::transport::Channel>,
    ac: reapi::action_cache_client::ActionCacheClient<tonic::transport::Channel>,
}

impl Ctx {
    async fn new() -> Ctx {
        let temp = tempfile::TempDir::new().unwrap();
        let store_dir = temp.path().join("store");
        let config = Config::from_toml(&format!(
            "bind = \"127.0.0.1:0\"\nstore_dir = \"{}\"\n\n[[backends]]\nname = \"local\"\nendpoint = \"self\"\n",
            store_dir.display()
        ))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let store = Store::open(store_dir, config.max_blob_bytes, 1_700_000_000)
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
            cas: reapi::content_addressable_storage_client::ContentAddressableStorageClient::connect(
                endpoint.clone(),
            )
            .await
            .unwrap(),
            ex: reapi::execution_client::ExecutionClient::connect(endpoint.clone())
                .await
                .unwrap(),
            ac: reapi::action_cache_client::ActionCacheClient::connect(endpoint).await.unwrap(),
        }
    }

    fn digest(data: &[u8]) -> reapi::Digest {
        reapi::Digest {
            hash: DigestFunction::Sha256.hexdigest(data),
            size_bytes: data.len() as i64,
        }
    }

    async fn put(&mut self, data: Vec<u8>) -> reapi::Digest {
        let digest = Self::digest(&data);
        self.cas
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
        digest
    }

    /// Stage one input file and upload the Directory + Command + Action
    /// protos, returning the action digest to execute.
    #[allow(clippy::too_many_arguments)]
    async fn put_action(
        &mut self,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        output_files: Vec<String>,
        output_paths: Vec<String>,
        input_name: &str,
        input_bytes: &[u8],
        platform: Option<reapi::Platform>,
        timeout: Option<prost_types::Duration>,
        do_not_cache: bool,
    ) -> reapi::Digest {
        let file_digest = self.put(input_bytes.to_vec()).await;
        let mut dir_bytes = Vec::new();
        reapi::Directory {
            files: vec![reapi::FileNode {
                name: input_name.to_string(),
                digest: Some(file_digest),
                is_executable: false,
                node_properties: None,
            }],
            directories: Vec::new(),
            symlinks: Vec::new(),
            node_properties: None,
        }
        .encode(&mut dir_bytes)
        .unwrap();
        let dir_digest = self.put(dir_bytes).await;
        let mut cmd_bytes = Vec::new();
        reapi::Command {
            arguments: argv,
            environment_variables: env
                .into_iter()
                .map(|(name, value)| reapi::command::EnvironmentVariable { name, value })
                .collect(),
            output_files,
            output_directories: Vec::new(),
            output_paths,
            platform: None,
            working_directory: String::new(),
            output_node_properties: Vec::new(),
            output_directory_format: 0,
        }
        .encode(&mut cmd_bytes)
        .unwrap();
        let command_digest = self.put(cmd_bytes).await;
        let mut action_bytes = Vec::new();
        reapi::Action {
            command_digest: Some(command_digest),
            input_root_digest: Some(dir_digest),
            timeout,
            do_not_cache,
            salt: Vec::new(),
            platform,
        }
        .encode(&mut action_bytes)
        .unwrap();
        self.put(action_bytes).await
    }

    /// Execute to completion (the daemon only ever sends one done
    /// operation) and unpack the single `ExecuteResponse`.
    async fn execute(&mut self, action: reapi::Digest) -> reapi::ExecuteResponse {
        let mut stream = self
            .ex
            .execute(reapi::ExecuteRequest {
                instance_name: String::new(),
                skip_cache_lookup: false,
                action_digest: Some(action),
                execution_policy: None,
                results_cache_policy: None,
                digest_function: 0,
                inline_stdout: false,
                inline_stderr: false,
                inline_output_files: Vec::new(),
            })
            .await
            .unwrap()
            .into_inner();
        let mut responses = Vec::new();
        while let Some(op) = stream.message().await.unwrap() {
            assert!(op.done, "reapid never streams incomplete operations");
            let longrunning::operation::Result::Response(any) = op.result.unwrap() else {
                panic!("execute returned an error operation");
            };
            assert_eq!(
                any.type_url,
                "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse"
            );
            responses.push(reapi::ExecuteResponse::decode(&any.value[..]).unwrap());
        }
        assert_eq!(responses.len(), 1, "exactly one done operation");
        responses.into_iter().next().unwrap()
    }
}

fn host_platform() -> reapi::Platform {
    reapi::Platform {
        properties: vec![
            reapi::platform::Property {
                name: "os".to_string(),
                value: std::env::consts::OS.to_string(),
            },
            reapi::platform::Property {
                name: "arch".to_string(),
                value: std::env::consts::ARCH.to_string(),
            },
        ],
    }
}

fn worker_name() -> String {
    format!("reapid-{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_runs_command_and_caches() {
    let mut ctx = Ctx::new().await;
    // `echo ran >> /tmp/...` is untestable here (side effects escape the
    // store); use a declared output file instead.
    let action = ctx
        .put_action(
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf '%s' hello-out > out.txt".to_string(),
            ],
            Vec::new(),
            vec!["out.txt".to_string()],
            Vec::new(),
            "in.txt",
            b"hello-in",
            Some(host_platform()),
            None,
            false,
        )
        .await;

    let first = ctx.execute(action.clone()).await;
    assert!(!first.cached_result);
    assert_eq!(first.status.unwrap().code, 0);
    let result = first.result.unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.execution_metadata.unwrap().worker, worker_name());
    assert_eq!(result.output_files.len(), 1);
    assert_eq!(result.output_files[0].path, "out.txt");
    let blob = ctx
        .cas
        .batch_read_blobs(reapi::BatchReadBlobsRequest {
            instance_name: String::new(),
            digests: vec![result.output_files[0].digest.clone().unwrap()],
            acceptable_compressors: vec![],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(blob.responses[0].data, b"hello-out");

    // Identical action: served from the action cache, command never re-runs.
    let second = ctx.execute(action).await;
    assert!(second.cached_result);
    assert_eq!(
        second.result.unwrap().output_files[0].digest,
        result.output_files[0].digest
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_discovers_output_paths() {
    let mut ctx = Ctx::new().await;
    let action = ctx
        .put_action(
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf '%s' nested > sub/dir/out.txt".to_string(),
            ],
            Vec::new(),
            Vec::new(),
            vec!["sub/dir/out.txt".to_string()],
            "in.txt",
            b"hello-in",
            Some(host_platform()),
            None,
            false,
        )
        .await;
    let response = ctx.execute(action).await;
    assert!(!response.cached_result);
    let result = response.result.unwrap();
    assert_eq!(result.exit_code, 0);
    // A nested file surfaces as its own output (auto-detected kind).
    assert_eq!(result.output_files.len(), 1);
    assert_eq!(result.output_files[0].path, "sub/dir/out.txt");
    let blob = ctx
        .cas
        .batch_read_blobs(reapi::BatchReadBlobsRequest {
            instance_name: String::new(),
            digests: vec![result.output_files[0].digest.clone().unwrap()],
            acceptable_compressors: vec![],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(blob.responses[0].data, b"nested");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_records_failures_without_caching() {
    let mut ctx = Ctx::new().await;
    let action = ctx
        .put_action(
            vec!["/bin/sh".to_string(), "-c".to_string(), "exit 3".to_string()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            "in.txt",
            b"hello-in",
            Some(host_platform()),
            None,
            true,
        )
        .await;
    let response = ctx.execute(action.clone()).await;
    assert!(!response.cached_result);
    let result = response.result.unwrap();
    assert_eq!(result.exit_code, 3);
    assert_eq!(result.execution_metadata.unwrap().worker, worker_name());
    // `do_not_cache`: the action ran but nothing was stored.
    let err = ctx
        .ac
        .get_action_result(reapi::GetActionResultRequest {
            instance_name: String::new(),
            action_digest: Some(action),
            inline_stdout: false,
            inline_stderr: false,
            inline_output_files: Vec::new(),
            digest_function: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_does_not_cache_nonzero_exits() {
    let mut ctx = Ctx::new().await;
    let action = ctx
        .put_action(
            vec!["/bin/sh".to_string(), "-c".to_string(), "exit 3".to_string()],
            Vec::new(),
            Vec::new(),
            Vec::new(),
            "in.txt",
            b"hello-in",
            Some(host_platform()),
            None,
            false,
        )
        .await;
    let first = ctx.execute(action.clone()).await;
    assert!(!first.cached_result);
    assert_eq!(first.result.unwrap().exit_code, 3);
    // A nonzero exit is never stored: the second run executes again
    // instead of replaying the recorded failure.
    let second = ctx.execute(action.clone()).await;
    assert!(!second.cached_result);
    assert_eq!(second.result.unwrap().exit_code, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_rejects_wrong_platform() {
    let mut ctx = Ctx::new().await;
    let action = ctx
        .put_action(
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo -n unreachable > out.txt".to_string(),
            ],
            Vec::new(),
            vec!["out.txt".to_string()],
            Vec::new(),
            "in.txt",
            b"hello-in",
            Some(reapi::Platform {
                properties: vec![reapi::platform::Property {
                    name: "os".to_string(),
                    value: "definitely-not-an-os".to_string(),
                }],
            }),
            None,
            false,
        )
        .await;
    let err = ctx
        .ex
        .execute(reapi::ExecuteRequest {
            instance_name: String::new(),
            skip_cache_lookup: false,
            action_digest: Some(action),
            execution_policy: None,
            results_cache_policy: None,
            digest_function: 0,
            inline_stdout: false,
            inline_stderr: false,
            inline_output_files: Vec::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("os"), "{}", err.message());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_ignores_unknown_platform_keys() {
    // Forward compatibility: requirements the daemon does not know must
    // not block execution.
    let mut ctx = Ctx::new().await;
    let action = ctx
        .put_action(
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf '%s' future-proof > out.txt".to_string(),
            ],
            Vec::new(),
            vec!["out.txt".to_string()],
            Vec::new(),
            "in.txt",
            b"hello-in",
            Some(reapi::Platform {
                properties: vec![
                    reapi::platform::Property {
                        name: "os".to_string(),
                        value: std::env::consts::OS.to_string(),
                    },
                    reapi::platform::Property {
                        name: "some-future-key".to_string(),
                        value: "whatever".to_string(),
                    },
                ],
            }),
            None,
            false,
        )
        .await;
    let response = ctx.execute(action).await;
    assert!(!response.cached_result);
    assert_eq!(response.result.unwrap().exit_code, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_missing_input_fails_precondition() {
    let mut ctx = Ctx::new().await;
    // A command blob that exists, pointing at an input root that does not.
    let mut cmd_bytes = Vec::new();
    reapi::Command {
        arguments: vec!["/bin/sh".to_string(), "-c".to_string(), "exit 0".to_string()],
        environment_variables: Vec::new(),
        output_files: Vec::new(),
        output_directories: Vec::new(),
        output_paths: Vec::new(),
        platform: None,
        working_directory: String::new(),
        output_node_properties: Vec::new(),
        output_directory_format: 0,
    }
    .encode(&mut cmd_bytes)
    .unwrap();
    let command_digest = ctx.put(cmd_bytes).await;
    let mut action_bytes = Vec::new();
    reapi::Action {
        command_digest: Some(command_digest),
        input_root_digest: Some(reapi::Digest {
            hash: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_string(),
            size_bytes: 12345,
        }),
        timeout: None,
        do_not_cache: false,
        salt: Vec::new(),
        platform: None,
    }
    .encode(&mut action_bytes)
    .unwrap();
    let action_digest = ctx.put(action_bytes).await;
    let err = ctx
        .ex
        .execute(reapi::ExecuteRequest {
            instance_name: String::new(),
            skip_cache_lookup: false,
            action_digest: Some(action_digest),
            execution_policy: None,
            results_cache_policy: None,
            digest_function: 0,
            inline_stdout: false,
            inline_stderr: false,
            inline_output_files: Vec::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(err.message().contains("missing inputs"), "{}", err.message());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_timeout_kills_and_reports() {
    let mut ctx = Ctx::new().await;
    let action = ctx
        .put_action(
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "sleep 30".to_string(),
            ],
            // Explicit PATH: the action env is passed through verbatim.
            vec![("PATH".to_string(), "/usr/bin:/bin".to_string())],
            vec!["out.txt".to_string()],
            Vec::new(),
            "in.txt",
            b"hello-in",
            Some(host_platform()),
            Some(prost_types::Duration {
                seconds: 0,
                nanos: 1,
            }),
            false,
        )
        .await;
    let mut stream = ctx
        .ex
        .execute(reapi::ExecuteRequest {
            instance_name: String::new(),
            skip_cache_lookup: false,
            action_digest: Some(action.clone()),
            execution_policy: None,
            results_cache_policy: None,
            digest_function: 0,
            inline_stdout: false,
            inline_stderr: false,
            inline_output_files: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();
    let op = stream.message().await.unwrap().unwrap();
    assert!(op.done);
    let longrunning::operation::Result::Response(any) = op.result.unwrap() else {
        panic!("expected response");
    };
    let response = reapi::ExecuteResponse::decode(&any.value[..]).unwrap();
    assert!(!response.cached_result);
    assert_eq!(
        response.status.unwrap().code,
        tonic::Code::DeadlineExceeded as i32
    );
    assert!(response.result.is_none());
    // Timed-out executions are never cached.
    let err = ctx
        .ac
        .get_action_result(reapi::GetActionResultRequest {
            instance_name: String::new(),
            action_digest: Some(action),
            inline_stdout: false,
            inline_stderr: false,
            inline_output_files: Vec::new(),
            digest_function: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_skip_cache_lookup_forces_rerun() {
    let mut ctx = Ctx::new().await;
    let action = ctx
        .put_action(
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf '%s' skip-me > out.txt".to_string(),
            ],
            Vec::new(),
            vec!["out.txt".to_string()],
            Vec::new(),
            "in.txt",
            b"hello-in",
            Some(host_platform()),
            None,
            false,
        )
        .await;
    // First run stores the result ...
    let first = ctx.execute(action.clone()).await;
    assert!(!first.cached_result);
    // ... but an explicit skip bypasses the lookup and runs again.
    let mut stream = ctx
        .ex
        .execute(reapi::ExecuteRequest {
            instance_name: String::new(),
            skip_cache_lookup: true,
            action_digest: Some(action),
            execution_policy: None,
            results_cache_policy: None,
            digest_function: 0,
            inline_stdout: false,
            inline_stderr: false,
            inline_output_files: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();
    let op = stream.message().await.unwrap().unwrap();
    assert!(op.done);
    let longrunning::operation::Result::Response(any) = op.result.unwrap() else {
        panic!("expected response");
    };
    let response = reapi::ExecuteResponse::decode(&any.value[..]).unwrap();
    assert!(!response.cached_result);
    assert_eq!(response.result.unwrap().exit_code, 0);
}

/// A staged tool invoked through a relative `argv[0]` — directly and
/// through a symlink, the shape buck2 uses for toolchain programs
/// (e.g. `buck-out/.../bin/python3 -> python3.14`).
///
/// Regression test: the daemon once resolved the executable before
/// staging the inputs, so every action whose tool lives in the input
/// tree failed with "not staged" while system binaries (`/bin/sh`)
/// kept passing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_runs_staged_relative_tool_and_symlink() {
    let mut ctx = Ctx::new().await;

    let script_digest = ctx.put(b"#!/bin/sh\nprintf '%s' ran > marker.txt\n".to_vec()).await;
    let mut tools_bytes = Vec::new();
    reapi::Directory {
        files: vec![reapi::FileNode {
            name: "prog".to_string(),
            digest: Some(script_digest),
            is_executable: true,
            node_properties: None,
        }],
        directories: Vec::new(),
        symlinks: vec![reapi::SymlinkNode {
            name: "link".to_string(),
            target: "prog".to_string(),
            node_properties: None,
        }],
        node_properties: None,
    }
    .encode(&mut tools_bytes)
    .unwrap();
    let tools_digest = ctx.put(tools_bytes).await;
    let mut root_bytes = Vec::new();
    reapi::Directory {
        files: Vec::new(),
        directories: vec![reapi::DirectoryNode {
            name: "tools".to_string(),
            digest: Some(tools_digest),
        }],
        symlinks: Vec::new(),
        node_properties: None,
    }
    .encode(&mut root_bytes)
    .unwrap();
    let root_digest = ctx.put(root_bytes).await;

    for argv0 in ["tools/prog", "tools/link"] {
        let mut cmd_bytes = Vec::new();
        reapi::Command {
            arguments: vec![argv0.to_string()],
            environment_variables: Vec::new(),
            output_files: Vec::new(),
            output_directories: Vec::new(),
            output_paths: vec!["marker.txt".to_string()],
            platform: None,
            working_directory: String::new(),
            output_node_properties: Vec::new(),
            output_directory_format: 0,
        }
        .encode(&mut cmd_bytes)
        .unwrap();
        let command_digest = ctx.put(cmd_bytes).await;
        let mut action_bytes = Vec::new();
        reapi::Action {
            command_digest: Some(command_digest),
            input_root_digest: Some(root_digest.clone()),
            timeout: None,
            do_not_cache: false,
            salt: Vec::new(),
            platform: Some(host_platform()),
        }
        .encode(&mut action_bytes)
        .unwrap();
        let action = ctx.put(action_bytes).await;

        let response = ctx.execute(action).await;
        assert_eq!(response.status.unwrap().code, 0, "argv0 = {argv0}");
        let result = response.result.unwrap();
        assert_eq!(result.exit_code, 0, "argv0 = {argv0}");
        assert_eq!(result.output_files.len(), 1, "argv0 = {argv0}");
        assert_eq!(result.output_files[0].path, "marker.txt");
        let blob = ctx
            .cas
            .batch_read_blobs(reapi::BatchReadBlobsRequest {
                instance_name: String::new(),
                digests: vec![result.output_files[0].digest.clone().unwrap()],
                acceptable_compressors: vec![],
                digest_function: 0,
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(blob.responses[0].data, b"ran", "argv0 = {argv0}");
    }
}

/// Host execution inherits the daemon's environment (it runs in-process
/// here, so the test process's own variables stand in for it) with the
/// action's declared variables winning.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn execute_inherits_daemon_env_with_client_override() {
    unsafe {
        std::env::set_var("REAPID_TEST_INHERIT", "from-daemon");
        std::env::set_var("REAPID_TEST_OVERRIDE", "from-daemon");
    }
    let mut ctx = Ctx::new().await;
    let action = ctx
        .put_action(
            vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "printf '%s:%s:%s' \"$REAPID_TEST_INHERIT\" \"$REAPID_TEST_OVERRIDE\" \"$REAPID_TEST_CLIENT_ONLY\" > out.txt"
                    .to_string(),
            ],
            vec![
                ("REAPID_TEST_OVERRIDE".to_string(), "from-client".to_string()),
                ("REAPID_TEST_CLIENT_ONLY".to_string(), "client".to_string()),
            ],
            vec!["out.txt".to_string()],
            Vec::new(),
            "in.txt",
            b"hello-in",
            Some(host_platform()),
            None,
            false,
        )
        .await;

    let response = ctx.execute(action).await;
    assert_eq!(response.status.unwrap().code, 0);
    let result = response.result.unwrap();
    assert_eq!(result.exit_code, 0);
    let blob = ctx
        .cas
        .batch_read_blobs(reapi::BatchReadBlobsRequest {
            instance_name: String::new(),
            digests: vec![result.output_files[0].digest.clone().unwrap()],
            acceptable_compressors: vec![],
            digest_function: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(blob.responses[0].data, b"from-daemon:from-client:client");
}
