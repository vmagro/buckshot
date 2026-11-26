//! The `reapid` REAPI server as a `buckshot` subcommand: actions run as
//! subprocesses on this host; results and blobs live in a directory blob
//! store. See `remote_execution/reapid/README.md`.
//!
//! Also hosts the offline `prune` subcommand, which deletes every store
//! entry the most recent daemon process never touched (see
//! `reapi_cache::prune`). Stop the daemon before pruning: the pruner walks
//! files directly and must not race a live index.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use clap::Parser;
use clap::Subcommand;

#[derive(Parser)]
#[command(about = "Run the reapid REAPI server (local store and/or multiplexed backends)")]
pub struct ReapidArgs {
    /// Path to the TOML config file (serve mode; omitted for `prune`).
    #[arg(long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<ReapidCommand>,
}

#[derive(Subcommand)]
enum ReapidCommand {
    /// Delete store entries untouched during the most recent daemon
    /// process's lifetime, so the uploaded cache holds only that run's
    /// working set instead of growing without bound. Stop the daemon
    /// first, then prune, then upload the cache.
    Prune(PruneArgs),
}

#[derive(Parser)]
pub struct PruneArgs {
    /// Store root to prune (the daemon's `store_dir`).
    #[arg(long)]
    store_dir: PathBuf,

    /// Only keep entries touched at or after this unix time. Defaults to
    /// the store's `.reapid-birth` marker, written by the most recent
    /// daemon process at startup.
    #[arg(long)]
    since: Option<u64>,

    /// Report what would be deleted without deleting anything.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
}

pub async fn run(args: ReapidArgs) -> anyhow::Result<()> {
    if let Some(ReapidCommand::Prune(prune)) = args.command {
        return run_prune(prune).await;
    }
    let Some(config) = args.config else {
        return Err(anyhow::anyhow!(
            "reapid serve mode needs --config <file> (or run `reapid prune --help`)"
        ));
    };
    run_serve(&config).await
}

async fn run_prune(args: PruneArgs) -> anyhow::Result<()> {
    let (since_secs, source) = match args.since {
        Some(since) => (since, "--since".to_string()),
        None => (
            reapi_cache::store::read_birth(&args.store_dir).map_err(|e| {
                anyhow::anyhow!("{e} (is this a reapid store? pass --since explicitly)")
            })?,
            reapi_cache::store::BIRTH_FILE.to_string(),
        ),
    };
    let since = UNIX_EPOCH + Duration::from_secs(since_secs);
    let now = SystemTime::now();
    if since > now {
        return Err(anyhow::anyhow!(
            "--since {since_secs} is in the future; refusing to prune"
        ));
    }
    let stats =
        reapi_cache::prune::prune_untouched(&args.store_dir, since, args.dry_run)
            .await
            .map_err(|e| {
                anyhow::anyhow!("prune {}: {e}", args.store_dir.display())
            })?;
    println!(
        "reapid prune{} {}: scanned {} entries, kept {}, removed {}, freed {} bytes (since {since_secs} from {source})",
        if args.dry_run { " (dry run)" } else { "" },
        args.store_dir.display(),
        stats.scanned,
        stats.kept,
        stats.removed,
        stats.freed_bytes,
        since_secs = since_secs,
        source = source,
    );
    Ok(())
}

async fn run_serve(config: &std::path::Path) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let text = std::fs::read_to_string(config)
        .map_err(|e| anyhow::anyhow!("read {}: {e}", config.display()))?;
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
