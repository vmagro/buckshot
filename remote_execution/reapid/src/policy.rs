//! Eviction policy: TTL expiry plus quota-driven LRU with pins.
//!
//! An eviction pass (run after every mutating RPC and once at startup):
//!
//! 1. deletes entries whose matching rule sets `ttl_secs` and whose age
//!    exceeds it, even when under quota (pinned entries excepted);
//! 2. while over `max_store_bytes`, deletes the least-recently-used
//!    unpinned entry (ties broken by hash for determinism).
//!
//! Eviction is always safe: `FindMissingBlobs` reports evicted digests as
//! missing and clients re-upload them.

use crate::config::Rule;
use crate::store::BlobMeta;
use crate::store::EntryKey;
use crate::store::Store;

/// Match `text` against a glob with `*` (any run, including empty) and `?`
/// (exactly one char). No escaping, no character classes: cache policy
/// patterns do not need them.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let (px, tx) = (pattern.as_bytes(), text.as_bytes());
    let (mut star, mut mark) = (None, 0);
    let (mut pi, mut ti) = (0, 0);
    while ti < tx.len() {
        if pi < px.len() && (px[pi] == b'?' || px[pi] == tx[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < px.len() && px[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < px.len() && px[pi] == b'*' {
        pi += 1;
    }
    pi == px.len()
}

fn any_match(patterns: &[String], value: Option<&str>) -> bool {
    if patterns.is_empty() {
        return true;
    }
    value.is_some_and(|v| patterns.iter().any(|p| glob_match(p, v)))
}

/// True if `rule` matches this entry.
pub fn rule_matches(rule: &Rule, meta: &BlobMeta) -> bool {
    if !any_match(&rule.target, meta.hints.target_id.as_deref()) {
        return false;
    }
    if !any_match(&rule.mnemonic, meta.hints.mnemonic.as_deref()) {
        return false;
    }
    if !any_match(&rule.invocation, meta.hints.tool_invocation_id.as_deref()) {
        return false;
    }
    if rule.min_size_bytes.is_some_and(|min| meta.size_bytes < min) {
        return false;
    }
    if rule.max_size_bytes.is_some_and(|max| meta.size_bytes > max) {
        return false;
    }
    true
}

pub enum Verdict {
    /// Never auto-evict.
    Pin,
    /// Expire past this age (unix-seconds TTL).
    Ttl(u64),
    /// Default LRU pool.
    Default,
}

/// First matching rule wins; no match means the default pool.
pub fn evaluate(rules: &[Rule], meta: &BlobMeta) -> Verdict {
    for rule in rules {
        if rule_matches(rule, meta) {
            if rule.pin {
                return Verdict::Pin;
            }
            if let Some(ttl) = rule.ttl_secs {
                return Verdict::Ttl(ttl);
            }
            return Verdict::Default;
        }
    }
    Verdict::Default
}

/// Run one eviction pass. Returns the number of entries deleted.
pub async fn evict(store: &Store, rules: &[Rule], max_store_bytes: u64, now: u64) -> usize {
    let snapshot = store.snapshot();
    let mut deleted = 0;
    let mut total: u64 = snapshot.iter().map(|(_, m)| m.footprint()).sum();

    // 1. TTL expiry, even under quota.
    for (key, meta) in &snapshot {
        if matches!(evaluate(rules, meta), Verdict::Ttl(ttl) if now.saturating_sub(meta.first_seen) > ttl)
        {
            total = total.saturating_sub(meta.footprint());
            store.remove(key).await;
            deleted += 1;
        }
    }
    if total <= max_store_bytes {
        return deleted;
    }

    // 2. LRU over quota, pins excluded.
    let mut candidates: Vec<(&EntryKey, &BlobMeta)> = snapshot
        .iter()
        .map(|(k, m)| (k, m))
        .filter(|(_, m)| !matches!(evaluate(rules, m), Verdict::Pin))
        .collect();
    candidates.sort_by(|a, b| {
        a.1.last_access
            .cmp(&b.1.last_access)
            .then_with(|| a.1.seq.cmp(&b.1.seq))
            .then_with(|| a.0.hash.cmp(&b.0.hash))
    });
    for (key, meta) in candidates {
        if total <= max_store_bytes {
            break;
        }
        total = total.saturating_sub(meta.footprint());
        store.remove(key).await;
        deleted += 1;
    }
    // Callers serialize passes (see Shared::evict_lock); even so, `remove`
    // is idempotent, so a stale snapshot entry can never corrupt anything.
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::EntryKind;
    use crate::store::Hints;

    fn meta_with(hints: Hints, size: u64, first_seen: u64) -> BlobMeta {
        BlobMeta {
            size_bytes: size,
            compressed_size_bytes: None,
            first_seen,
            last_access: first_seen,
            access_count: 0,
            seq: 0,
            hints,
        }
    }

    #[test]
    fn globs() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("Rustc*", "RustcFoo"));
        assert!(!glob_match("Rustc*", "rustc"));
        assert!(glob_match("//foo/...", "//foo/..."));
        assert!(glob_match("//foo/*", "//foo/bar"));
        assert!(!glob_match("//foo/*", "//food/bar"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("*Test*", "myTestTarget"));
        assert!(!glob_match("", "x"));
        assert!(glob_match("", ""));
    }

    #[test]
    fn rule_matching() {
        let rule = Rule {
            name: "r".to_string(),
            target: vec!["//foo/*".to_string()],
            mnemonic: vec!["Rustc*".to_string()],
            invocation: vec![],
            min_size_bytes: Some(10),
            max_size_bytes: None,
            pin: true,
            ttl_secs: None,
        };
        let meta = meta_with(
            Hints {
                target_id: Some("//foo/bar".to_string()),
                mnemonic: Some("Rustc".to_string()),
                ..Hints::default()
            },
            100,
            0,
        );
        assert!(rule_matches(&rule, &meta));
        assert!(!rule_matches(
            &rule,
            &meta_with(Hints { mnemonic: Some("Rustc".to_string()), ..Hints::default() }, 100, 0)
        ));
        assert!(!rule_matches(&rule, &meta_with(meta.hints.clone(), 5, 0)));
        // Empty rule matches everything.
        let catch_all = Rule {
            name: "all".to_string(),
            target: vec![],
            mnemonic: vec![],
            invocation: vec![],
            min_size_bytes: None,
            max_size_bytes: None,
            pin: false,
            ttl_secs: Some(1),
        };
        assert!(rule_matches(&catch_all, &meta));
    }

    fn test_root(name: &str) -> std::path::PathBuf {
        let mut dir = std::env::temp_dir();
        dir.push(format!("reapid-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn eviction_prefers_lru_and_honors_pins() {
        use crate::digest::DigestFunction;
        let root = test_root("evict");
        let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
        // Three 10-byte blobs with distinct ages and hints.
        for (name, mnemonic, now) in [("a", "Old", 1000), ("b", "New", 2000), ("c", "Keep", 1500)] {
            let data = vec![name.as_bytes()[0]; 10];
            let hash = DigestFunction::Sha256.hexdigest(&data);
            store
                .cas_put(
                    &data,
                    &hash,
                    10,
                    1,
                    &Hints {
                        mnemonic: Some(mnemonic.to_string()),
                        ..Hints::default()
                    },
                    now,
                )
                .await
                .unwrap();
        }
        assert_eq!(store.total_bytes(), 30);
        let rules = vec![Rule {
            name: "keep".to_string(),
            target: vec![],
            mnemonic: vec!["Keep".to_string()],
            invocation: vec![],
            min_size_bytes: None,
            max_size_bytes: None,
            pin: true,
            ttl_secs: None,
        }];
        // Quota 20: evicts oldest unpinned ("a"), keeps "b" and pinned "c".
        assert_eq!(evict(&store, &rules, 20, 3000).await, 1);
        assert_eq!(store.len(), 2);
        let hashes: Vec<String> = store.snapshot().iter().map(|(k, _)| k.hash.clone()).collect();
        let hash_a = DigestFunction::Sha256.hexdigest(&vec![b'a'; 10]);
        assert!(!hashes.contains(&hash_a));
        // Quota 5: evicts "b" but the pinned entry survives over quota.
        assert_eq!(evict(&store, &rules, 5, 3000).await, 1);
        assert_eq!(store.len(), 1);
        let remaining = &store.snapshot()[0];
        assert_eq!(remaining.0.kind, EntryKind::Cas);
        assert_eq!(remaining.1.hints.mnemonic.as_deref(), Some("Keep"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn ttl_expires_under_quota() {
        use crate::digest::DigestFunction;
        let root = test_root("ttl");
        let store = Store::open(root.clone(), 1 << 30, 1000).await.unwrap();
        let data = vec![1u8; 10];
        let hash = DigestFunction::Sha256.hexdigest(&data);
        store.cas_put(&data, &hash, 10, 1, &Hints::default(), 1000).await.unwrap();
        let rules = vec![Rule {
            name: "short".to_string(),
            target: vec![],
            mnemonic: vec![],
            invocation: vec![],
            min_size_bytes: None,
            max_size_bytes: None,
            pin: false,
            ttl_secs: Some(100),
        }];
        // Age 50: kept. Age 101: expired despite ample quota.
        assert_eq!(evict(&store, &rules, 1 << 30, 1050).await, 0);
        assert_eq!(evict(&store, &rules, 1 << 30, 1101).await, 1);
        assert!(store.is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
