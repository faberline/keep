//! Cluster topology and memory-budget resolution for the keep server.

use anyhow::Result;

/// Parse a byte size string with optional binary/decimal suffixes (`Ki`, `Mi`,
/// `Gi`, `Ti`, `KB`, `MB`, `GB`, `TB`, `K`, `M`, `G`, `T`) or raw integer bytes.
pub(crate) fn parse_byte_size(raw: &str) -> Result<usize> {
    let s = raw.trim();
    if s.is_empty() {
        anyhow::bail!("empty byte size");
    }
    let split_idx = s
        .find(|c: char| !c.is_ascii_digit() && c != '_')
        .unwrap_or(s.len());
    let num_part = s[..split_idx].replace('_', "");
    let unit_part = s[split_idx..].trim().to_ascii_lowercase();
    let value: u64 = num_part
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid byte size `{raw}`"))?;
    let multiplier: u64 = match unit_part.as_str() {
        "" | "b" => 1,
        "k" | "ki" | "kib" => 1024,
        "kb" => 1_000,
        "m" | "mi" | "mib" => 1024 * 1024,
        "mb" => 1_000_000,
        "g" | "gi" | "gib" => 1024 * 1024 * 1024,
        "gb" => 1_000_000_000,
        "t" | "ti" | "tib" => 1024_u64.pow(4),
        "tb" => 1_000_000_000_000,
        _ => anyhow::bail!("unsupported byte size suffix in `{raw}`"),
    };
    let bytes = value
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("byte size overflow in `{raw}`"))?;
    usize::try_from(bytes).map_err(|_| anyhow::anyhow!("byte size exceeds usize in `{raw}`"))
}

/// Detect container memory limit (bytes) from Linux cgroup v2 or cgroup v1.
/// Returns `None` when not running in a memory-constrained cgroup.
pub(crate) fn detect_cgroup_memory_limit() -> Option<usize> {
    for path in [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ] {
        if let Ok(raw) = std::fs::read_to_string(path) {
            let trimmed = raw.trim();
            if trimmed == "max" || trimmed.is_empty() {
                continue;
            }
            if let Ok(bytes) = trimmed.parse::<u64>() {
                // Ignore "unlimited" sentinel values (>= 1 EiB / PAGE_COUNTER_MAX).
                if bytes > 0 && bytes < (1u64 << 56) {
                    if let Ok(val) = usize::try_from(bytes) {
                        return Some(val);
                    }
                }
            }
        }
    }
    None
}

/// Resolve the effective `max_memory` budget in bytes from an explicit CLI/env
/// override or a ratio of the detected cgroup memory limit.
pub(crate) fn resolve_maxmemory(
    explicit: Option<&str>,
    ratio: f64,
    cgroup_limit: Option<usize>,
) -> Result<Option<usize>> {
    if let Some(raw) = explicit {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            let bytes = parse_byte_size(trimmed)?;
            return Ok((bytes > 0).then_some(bytes));
        }
    }
    if let Some(limit) = cgroup_limit {
        let clamped_ratio = ratio.clamp(0.10, 0.98);
        let budget = ((limit as f64) * clamped_ratio) as usize;
        if budget > 0 {
            return Ok(Some(budget));
        }
    }
    Ok(None)
}

pub(crate) fn derive_cluster_config(args: &crate::serve::ServeArgs) -> Result<keep::ClusterConfig> {
    if let Some(cluster) = local_topology_override(args) {
        return Ok(cluster);
    }
    if !has_downward_api_topology() {
        return Ok(keep::ClusterConfig::new(
            args.node_id,
            args.node_count,
            args.shard_count,
            args.peers.clone(),
        ));
    }

    let headless =
        std::env::var("KEEP_HEADLESS_SERVICE").unwrap_or_else(|_| "keep-headless".to_string());
    let dims = raft_runtime::ClusterDims::from_env()?;
    let topo = raft_runtime::ClusterTopology::from_env("keep", &headless, args.port, "KEEP_PEERS")?;
    let node_count = topo.replicas_per_shard.max(1) as usize;
    let mut peers = vec![String::new(); node_count];
    for (node, url) in topo.peers {
        if let Some(slot) = peers.get_mut(node as usize) {
            *slot = url;
        }
    }
    Ok(keep::ClusterConfig::from_shared_topology(
        topo.node_id as usize,
        node_count,
        dims.shard_count,
        peers,
    ))
}

fn local_topology_override(args: &crate::serve::ServeArgs) -> Option<keep::ClusterConfig> {
    let has_override = std::env::var_os("KEEP_NODE_ID").is_some()
        || std::env::var_os("KEEP_PEERS").is_some()
        || args.node_id != 0
        || !args.peers.is_empty();
    if !has_override {
        return None;
    }
    let node_id = env_parse("KEEP_NODE_ID").unwrap_or(args.node_id);
    let node_count = env_parse("KEEP_NODE_COUNT")
        .unwrap_or(args.node_count)
        .max(node_id + 1);
    let shard_count = env_parse("KEEP_SHARD_COUNT").unwrap_or(args.shard_count);
    let peers = std::env::var("KEEP_PEERS")
        .ok()
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_else(|| args.peers.clone());
    Some(keep::ClusterConfig::new(
        node_id,
        node_count,
        shard_count,
        peers,
    ))
}

fn env_parse<T>(key: &str) -> Option<T>
where
    T: std::str::FromStr,
{
    std::env::var(key).ok()?.parse().ok()
}

fn has_downward_api_topology() -> bool {
    [
        "POD_NAME",
        "SHARD_COUNT",
        "REPLICAS_PER_SHARD",
        "VOTER_COUNT",
    ]
    .iter()
    .all(|key| std::env::var_os(key).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Cli;
    use clap::Parser;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    const TOPOLOGY_ENV: &[&str] = &[
        "POD_NAME",
        "SHARD_COUNT",
        "REPLICAS_PER_SHARD",
        "VOTER_COUNT",
        "KEEP_HEADLESS_SERVICE",
        "KEEP_NODE_ID",
        "KEEP_NODE_COUNT",
        "KEEP_SHARD_COUNT",
        "KEEP_PEERS",
    ];

    fn clear_topology_env() {
        for key in TOPOLOGY_ENV {
            std::env::remove_var(key);
        }
    }

    fn serve_args(extra: &[&str]) -> crate::serve::ServeArgs {
        let mut argv = vec!["keep"];
        argv.extend_from_slice(extra);
        Cli::try_parse_from(argv).expect("serve args parse").serve
    }

    #[test]
    fn cluster_topology_defaults_to_single_node() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_topology_env();

        let cluster = derive_cluster_config(&serve_args(&[])).unwrap();

        assert_eq!(cluster.node_id, 0);
        assert_eq!(cluster.node_count, 1);
        assert_eq!(cluster.shard_count, 1);
        assert!(cluster.peers.is_empty());
    }

    #[test]
    fn cluster_topology_keeps_local_override_precedence() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_topology_env();
        std::env::set_var("POD_NAME", "keep-2");
        std::env::set_var("SHARD_COUNT", "1");
        std::env::set_var("REPLICAS_PER_SHARD", "3");
        std::env::set_var("VOTER_COUNT", "3");
        std::env::set_var("KEEP_NODE_ID", "1");
        std::env::set_var("KEEP_PEERS", "127.0.0.1:7117,127.0.0.1:7118");

        let cluster = derive_cluster_config(&serve_args(&[])).unwrap();

        assert_eq!(cluster.node_id, 1);
        assert_eq!(cluster.node_count, 2);
        assert_eq!(cluster.shard_count, 2);
        assert_eq!(
            cluster.peers,
            vec!["127.0.0.1:7117".to_string(), "127.0.0.1:7118".to_string()]
        );
        clear_topology_env();
    }

    #[test]
    fn cluster_topology_uses_downward_api_when_no_local_override() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_topology_env();
        std::env::set_var("POD_NAME", "keep-2");
        std::env::set_var("SHARD_COUNT", "1");
        std::env::set_var("REPLICAS_PER_SHARD", "3");
        std::env::set_var("VOTER_COUNT", "3");
        std::env::set_var("KEEP_HEADLESS_SERVICE", "keep-headless");

        let cluster = derive_cluster_config(&serve_args(&[])).unwrap();

        assert_eq!(cluster.node_id, 2);
        assert_eq!(cluster.node_count, 3);
        assert_eq!(cluster.shard_count, 1);
        assert_eq!(cluster.peers[0], "http://keep-0.keep-headless:7117");
        assert_eq!(cluster.peers[1], "http://keep-1.keep-headless:7117");
        assert_eq!(cluster.peers[2], "");
        clear_topology_env();
    }

    #[test]
    fn byte_size_and_maxmemory_resolution_work() {
        assert_eq!(parse_byte_size("1024").unwrap(), 1024);
        assert_eq!(parse_byte_size("512Ki").unwrap(), 512 * 1024);
        assert_eq!(parse_byte_size("256Mi").unwrap(), 256 * 1024 * 1024);
        assert_eq!(parse_byte_size("4Gi").unwrap(), 4 * 1024 * 1024 * 1024);

        // Explicit maxmemory takes precedence over cgroup limit.
        assert_eq!(
            resolve_maxmemory(Some("128Mi"), 0.80, Some(1024 * 1024 * 1024)).unwrap(),
            Some(128 * 1024 * 1024)
        );
        // "0" disables memory limit even if cgroup limit exists.
        assert_eq!(
            resolve_maxmemory(Some("0"), 0.80, Some(1024 * 1024 * 1024)).unwrap(),
            None
        );
        // Unset maxmemory uses cgroup limit * ratio.
        assert_eq!(
            resolve_maxmemory(None, 0.80, Some(1_000_000_000)).unwrap(),
            Some(800_000_000)
        );
    }
}
