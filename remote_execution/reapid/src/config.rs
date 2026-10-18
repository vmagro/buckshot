//! TOML configuration.
//!
//! Every instance declares where it serves from: `[[backends]]` lists other
//! `reapid` instances and/or `endpoint = "self"` (this daemon's own store) in
//! priority order. A pure cache is just `backends = [self]`; a pure proxy
//! lists only remotes and omits `store_dir`.
//!
//! ```toml
//! bind = "127.0.0.1:50051"
//! store_dir = "/var/cache/reapid"
//! max_store_bytes = 10737418240
//! max_batch_bytes = 4194304
//! max_blob_bytes = 1073741824
//!
//! [[backends]]
//! name = "office"
//! endpoint = "http://10.0.0.5:50051"
//!
//! [[backends]]
//! name = "local"
//! endpoint = "self"
//!
//! [[rules]]
//! name = "keep rust artifacts"
//! mnemonic = ["Rustc*"]
//! pin = true
//!
//! [[rules]]
//! name = "short-lived test outputs"
//! mnemonic = ["*Test*"]
//! ttl_secs = 86400
//! ```

use std::path::PathBuf;

use serde::Deserialize;

use crate::digest::DigestFunction;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// gRPC listen address, e.g. "127.0.0.1:50051".
    pub bind: String,
    /// Blob store root (created if missing). Required when any backend is
    /// `self`, rejected otherwise: without `self` nothing reads the store.
    #[serde(default)]
    pub store_dir: Option<PathBuf>,
    /// Where this instance serves from, in priority order (first wins). At
    /// least one entry is required; a pure cache lists a single `self`.
    #[serde(default)]
    pub backends: Vec<BackendConfig>,
    /// Digest functions to advertise in Capabilities (subset of
    /// sha256/sha1/blake3). The store accepts and verifies all three
    /// regardless; this only controls what clients are told to use.
    #[serde(default = "default_digest_functions")]
    pub digest_functions: Vec<String>,
    /// Total quota for stored bytes (canonical + compressed forms).
    /// Entries past the quota evict least-recently-used-first.
    #[serde(default = "default_max_store_bytes")]
    pub max_store_bytes: u64,
    /// Largest accepted BatchUpdateBlobs payload (also advertised).
    #[serde(default = "default_max_batch_bytes")]
    pub max_batch_bytes: u64,
    /// Largest single blob accepted on any path.
    #[serde(default = "default_max_blob_bytes")]
    pub max_blob_bytes: u64,
    /// Eviction policy rules, first match wins. Defaults to plain LRU.
    #[serde(default)]
    pub rules: Vec<Rule>,
}

fn default_digest_functions() -> Vec<String> {
    ["sha256", "sha1", "blake3"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn default_max_store_bytes() -> u64 {
    10 << 30
}

fn default_max_batch_bytes() -> u64 {
    4 << 20
}

fn default_max_blob_bytes() -> u64 {
    1 << 30
}

/// One multiplex backend. Selection walks the list in order and takes the
/// first backend whose circuit breaker currently admits traffic.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    /// Human name, used in logs and proxied error messages. Unique.
    pub name: String,
    /// `"self"` (serve from this daemon's own store; always reachable) or
    /// the `http(s)` URL of another `reapid`, e.g.
    /// `"http://10.0.0.5:50051"`.
    pub endpoint: String,
    /// Consecutive connectivity failures before the breaker opens and the
    /// backend stops receiving new builds. Ignored for `self`.
    #[serde(default = "default_max_consecutive_failures")]
    pub max_consecutive_failures: u32,
    /// Seconds an open breaker waits before admitting one trial build.
    /// Ignored for `self`.
    #[serde(default = "default_cooldown_secs")]
    pub cooldown_secs: u64,
}

fn default_max_consecutive_failures() -> u32 {
    3
}

fn default_cooldown_secs() -> u64 {
    30
}

impl BackendConfig {
    /// True for the `self` endpoint (this daemon's own store).
    pub fn is_self(&self) -> bool {
        self.endpoint == "self"
    }
}

/// One policy rule. All present matchers must match (absent matchers are
/// wildcards); a rule with no matchers matches every entry.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub name: String,
    /// Glob patterns over the producing target id (`*`, `?`).
    #[serde(default)]
    pub target: Vec<String>,
    /// Glob patterns over the action mnemonic (e.g. `Rustc*`).
    #[serde(default)]
    pub mnemonic: Vec<String>,
    /// Glob patterns over the tool invocation id.
    #[serde(default)]
    pub invocation: Vec<String>,
    #[serde(default)]
    pub min_size_bytes: Option<u64>,
    #[serde(default)]
    pub max_size_bytes: Option<u64>,
    /// Pinned entries are never auto-evicted, even past quota (and even
    /// if the same rule also sets `ttl_secs`: pin wins).
    #[serde(default)]
    pub pin: bool,
    /// Entries older than this are deleted on eviction passes even when the
    /// store is under quota.
    #[serde(default)]
    pub ttl_secs: Option<u64>,
}

/// A remote endpoint is an absolute `http`/`https` URL (host + port, no
/// path required). Anything else would fail at dial time; reject it here.
fn valid_remote_endpoint(endpoint: &str) -> bool {
    let Ok(parsed) = tonic::transport::Endpoint::from_shared(endpoint.to_owned()) else {
        return false;
    };
    matches!(parsed.uri().scheme_str(), Some("http" | "https"))
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Config, String> {
        let config: Config =
            toml::from_str(text).map_err(|e| format!("invalid config: {e}"))?;
        config.validate()
    }

    fn validate(self) -> Result<Config, String> {
        if self.bind.parse::<std::net::SocketAddr>().is_err() {
            return Err(format!("invalid bind address {:?}", self.bind));
        }
        if self.digest_functions.is_empty() {
            return Err("digest_functions must not be empty".to_string());
        }
        for name in &self.digest_functions {
            if DigestFunction::parse(name).is_none() {
                return Err(format!("unknown digest function {name:?}"));
            }
        }
        if self.max_batch_bytes == 0 {
            return Err("max_batch_bytes must be positive".to_string());
        }
        if self.backends.is_empty() {
            return Err("at least one [[backends]] entry is required".to_string());
        }
        let mut names = std::collections::HashSet::new();
        for backend in &self.backends {
            if backend.name.is_empty() {
                return Err("backend names must not be empty".to_string());
            }
            if !names.insert(backend.name.clone()) {
                return Err(format!("duplicate backend name {:?}", backend.name));
            }
            if !backend.is_self() && !valid_remote_endpoint(&backend.endpoint) {
                return Err(format!(
                    "backend {:?}: endpoint must be \"self\" or an http(s) URL, got {:?}",
                    backend.name, backend.endpoint
                ));
            }
            if backend.max_consecutive_failures == 0 {
                return Err(format!(
                    "backend {:?}: max_consecutive_failures must be positive",
                    backend.name
                ));
            }
        }
        // The local store exists exactly when something routes to it.
        let has_self = self.backends.iter().any(BackendConfig::is_self);
        if has_self && self.store_dir.is_none() {
            return Err("a \"self\" backend requires store_dir to be set".to_string());
        }
        if !has_self && self.store_dir.is_some() {
            return Err("store_dir is set but no backend is \"self\": nothing would read the store".to_string());
        }
        if !has_self && !self.rules.is_empty() {
            return Err("[[rules]] are set but no backend is \"self\": nothing would apply them".to_string());
        }
        Ok(self)
    }

    /// Advertised digest functions as proto enum values.
    pub fn proto_digest_functions(&self) -> Vec<i32> {
        self.digest_functions
            .iter()
            .filter_map(|name| DigestFunction::parse(name))
            .map(|f| f.to_proto())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let config = Config::from_toml(
            r#"
bind = "127.0.0.1:50051"
store_dir = "/tmp/reapid"
digest_functions = ["sha256"]

[[backends]]
name = "office"
endpoint = "http://10.0.0.5:50051"
max_consecutive_failures = 5
cooldown_secs = 10

[[backends]]
name = "local"
endpoint = "self"

[[rules]]
name = "keep rust"
mnemonic = ["Rustc*"]
pin = true

[[rules]]
name = "tiny ttl"
max_size_bytes = 100
ttl_secs = 60
"#,
        )
        .unwrap();
        assert_eq!(config.proto_digest_functions(), vec![1]);
        assert_eq!(config.rules.len(), 2);
        assert!(config.rules[0].pin);
        assert_eq!(config.rules[1].ttl_secs, Some(60));
        // Defaults apply.
        assert_eq!(config.max_batch_bytes, 4 << 20);
        assert_eq!(config.backends.len(), 2);
        assert_eq!(config.backends[0].max_consecutive_failures, 5);
        assert_eq!(config.backends[0].cooldown_secs, 10);
        assert!(config.backends[1].is_self());
        assert_eq!(config.backends[1].max_consecutive_failures, 3);
    }

    #[test]
    fn parses_pure_proxy() {
        let config = Config::from_toml(
            r#"
bind = "127.0.0.1:50051"

[[backends]]
name = "cloud"
endpoint = "https://cache.example.com:443"
"#,
        )
        .unwrap();
        assert!(config.store_dir.is_none());
        assert_eq!(config.backends.len(), 1);
    }

    #[test]
    fn rejects_bad_config() {
        // No backends at all.
        assert!(Config::from_toml("bind = \"127.0.0.1:1\"\nstore_dir = \"/tmp/x\"").is_err());
        let backends = "[[backends]]\nname = \"local\"\nendpoint = \"self\"\n";
        assert!(Config::from_toml(&format!("bind = \"x\"\nstore_dir = \"/tmp/x\"\n{backends}")).is_err());
        assert!(
            Config::from_toml(&format!(
                "bind = \"127.0.0.1:1\"\nstore_dir = \"/tmp/x\"\ndigest_functions = []\n{backends}"
            ))
            .is_err()
        );
        assert!(
            Config::from_toml(&format!(
                "bind = \"127.0.0.1:1\"\nstore_dir = \"/tmp/x\"\ndigest_functions = [\"md5\"]\n{backends}"
            ))
            .is_err()
        );
        assert!(
            Config::from_toml(&format!("bind = \"127.0.0.1:1\"\nstore_dir = \"/tmp/x\"\nbogus = 1\n{backends}"))
                .is_err()
        );
        // self without a store; store/rules without self.
        assert!(
            Config::from_toml(&format!("bind = \"127.0.0.1:1\"\n{backends}")).is_err(),
            "self requires store_dir"
        );
        assert!(
            Config::from_toml(
                "bind = \"127.0.0.1:1\"\nstore_dir = \"/tmp/x\"\n[[backends]]\nname = \"r\"\nendpoint = \"http://127.0.0.1:9\"\n"
            )
            .is_err(),
            "store without self"
        );
        assert!(
            Config::from_toml(
                "bind = \"127.0.0.1:1\"\n[[backends]]\nname = \"r\"\nendpoint = \"http://127.0.0.1:9\"\n[[rules]]\nname = \"x\"\n"
            )
            .is_err(),
            "rules without self"
        );
        // Malformed backends.
        for bad_backend in [
            // Duplicate names.
            "[[backends]]\nname = \"d\"\nendpoint = \"self\"\n[[backends]]\nname = \"d\"\nendpoint = \"self\"\n",
            // Neither self nor a URL.
            "[[backends]]\nname = \"r\"\nendpoint = \"10.0.0.5:50051\"\n",
            "[[backends]]\nname = \"r\"\nendpoint = \"grpc://10.0.0.5:50051\"\n",
            // Unknown field.
            "[[backends]]\nname = \"r\"\nendpoint = \"http://127.0.0.1:9\"\nhealth = \"ping\"\n",
            // Zero threshold.
            "[[backends]]\nname = \"r\"\nendpoint = \"http://127.0.0.1:9\"\nmax_consecutive_failures = 0\n",
        ] {
            assert!(
                Config::from_toml(&format!("bind = \"127.0.0.1:1\"\nstore_dir = \"/tmp/x\"\n{bad_backend}"))
                    .is_err(),
                "accepted {bad_backend:?}"
            );
        }
    }
}
