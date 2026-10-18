//! `reapid`: cache-only REAPI v2 server for buck2.
//!
//! Buck2 runs every action locally; this daemon serves only cache traffic
//! from a directory blob store. See `remote_execution/reapid/README.md`.

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;
use reapi_cache::config::Config;
use reapi_cache::services::Shared;

#[derive(Parser)]
#[command(name = "reapid", about = "Cache-only REAPI server: local store and/or multiplexed backends")]
struct Cli {
    /// Path to the TOML config file.
    #[arg(long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();
    let text = std::fs::read_to_string(&cli.config)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", cli.config.display()))?;
    let config = Config::from_toml(&text).map_err(|e| anyhow::anyhow!("{e}"))?;
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
    let shared = Arc::new(Shared::new(store, config, mux));
    // One pass at startup so a restarted daemon with a shrunk quota (or
    // expired TTLs) converges before serving.
    shared.evict_pass().await;
    tracing::info!("serving REAPI (CAS, ActionCache, Capabilities, ByteStream)");
    reapi_cache::services::serve(listener, shared)
        .await
        .map_err(|e| anyhow::anyhow!("serve: {e}"))?;
    Ok(())
}
