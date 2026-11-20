//! The `reapid` REAPI server as a `buckshot` subcommand: actions run as
//! subprocesses on this host; results and blobs live in a directory blob
//! store. See `remote_execution/reapid/README.md`.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

#[derive(Parser)]
#[command(about = "Run the reapid REAPI server (local store and/or multiplexed backends)")]
pub struct ReapidArgs {
    /// Path to the TOML config file.
    #[arg(long)]
    config: PathBuf,
}

pub async fn run(args: ReapidArgs) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let text = std::fs::read_to_string(&args.config)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", args.config.display()))?;
    let config =
        reapi_cache::config::Config::from_toml(&text).map_err(|e| anyhow::anyhow!("{e}"))?;
    let listener = tokio::net::TcpListener::bind(&config.bind)
        .await
        .map_err(|e| anyhow::anyhow!("bind {}: {e}", config.bind))?;
    let store = match &config.store_dir {
        Some(dir) => {
            tracing::info!(bind = %config.bind, store = %dir.display(), "opening store");
            let store = reapi_cache::store::Store::open(
                dir.clone(),
                config.max_blob_bytes,
                reapi_cache::services::now_unix(),
            )
            .await
            .map_err(|e| anyhow::anyhow!("open store: {e}"))?;
            tracing::info!(entries = store.len(), bytes = store.total_bytes(), "store ready");
            Some(store)
        }
        None => {
            tracing::info!(bind = %config.bind, "no store_dir: pure proxy, no local store");
            None
        }
    };
    let mux = reapi_cache::mux::Mux::new(
        &config.backends,
        reapi_cache::services::frame_bytes(config.max_batch_bytes),
    )
    .map_err(|e| anyhow::anyhow!("mux: {e}"))?;
    let shared = Arc::new(reapi_cache::services::Shared::new(store, config, mux));
    // One pass at startup so a restarted daemon with a shrunk quota (or
    // expired TTLs) converges before serving.
    shared.evict_pass().await;
    tracing::info!("serving REAPI (CAS, ActionCache, Capabilities, Execution, ByteStream)");
    reapi_cache::services::serve(listener, shared)
        .await
        .map_err(|e| anyhow::anyhow!("serve: {e}"))?;
    Ok(())
}
