//! Topology-aware advisory budgets for large indexing hosts.
//!
//! The planner is intentionally data-only: it reads Linux topology, compares
//! against the current conservative defaults, and reports advisory CPU/RAM
//! budgets without changing the live indexing controllers.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

pub const TOPOLOGY_BUDGET_SCHEMA_VERSION: &str = "1";

const GIB: u64 = 1024 * 1024 * 1024;
const DEFAULT_CACHE_BYTE_CAP_FALLBACK: usize = 64 * 1024 * 1024;
const DEFAULT_CACHE_BYTE_CAP_MEMORY_FRACTION_DENOMINATOR: u64 = 128;
const DEFAULT_CACHE_BYTE_CAP_CEILING: u64 = 2 * GIB;
const TOPOLOGY_CACHE_BYTE_CAP_CEILING: u64 = 8 * GIB;
const DEFAULT_MAX_INFLIGHT_FALLBACK: usize = 32 * 1024 * 1024;
const TOPOLOGY_MAX_INFLIGHT_CEILING: u64 = 512 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyBudgetPlan {
    pub schema_version: String,
    pub topology: TopologySnapshot,
    pub reserved_core_policy: ReservedCorePolicy,
    pub advisory_budgets: TopologyAdvisoryBudgets,
    pub current_defaults: TopologyPlannerDefaults,
    pub fallback_active: bool,
    pub decision_reason: String,
    pub proof_notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologySnapshot {
    pub source: TopologySource,
    pub topology_class: TopologyClass,
    pub logical_cpus: usize,
    pub physical_cores: usize,
    pub sockets: usize,
    pub numa_nodes: usize,
    pub llc_groups: usize,
    pub smt_threads_per_core: usize,
    pub memory_total_bytes: Option<u64>,
    pub memory_available_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologySource {
    LinuxSysfs,
    Fallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TopologyClass {
    Unknown,
    SingleSocket,
    SingleSocketSmt,
    ManyCoreSingleSocket,
    MultiSocketNuma,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservedCorePolicy {
    pub reserved_cores: usize,
    pub policy: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyAdvisoryBudgets {
    pub shard_builders: usize,
    pub merge_workers: usize,
    pub page_prep_workers: usize,
    pub semantic_batchers: usize,
    pub cache_cap_bytes: usize,
    pub max_inflight_bytes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopologyPlannerDefaults {
    pub available_parallelism: usize,
    pub reserved_cores: usize,
    pub shard_builders: usize,
    pub merge_workers: usize,
    pub page_prep_workers: usize,
    pub cache_cap_bytes: usize,
    pub max_inflight_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemorySnapshot {
    pub total_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
}

impl TopologyPlannerDefaults {
    pub fn conservative(
        available_parallelism: usize,
        reserved_cores: usize,
        shard_builders: usize,
        merge_workers: usize,
        page_prep_workers: usize,
        cache_cap_bytes: usize,
        max_inflight_bytes: usize,
    ) -> Self {
        Self {
            available_parallelism: available_parallelism.max(1),
            reserved_cores: reserved_cores.min(available_parallelism.saturating_sub(1)),
            shard_builders: shard_builders.max(1),
            merge_workers: merge_workers.max(1),
            page_prep_workers: page_prep_workers.max(1),
            cache_cap_bytes: cache_cap_bytes.max(1),
            max_inflight_bytes: max_inflight_bytes.max(1),
        }
    }

    fn from_current_process_with(memory: MemorySnapshot) -> Self {
        let pipeline = crate::indexer::lexical_rebuild_pipeline_settings_snapshot();
        Self::conservative(
            pipeline.available_parallelism,
            pipeline.reserved_cores,
            pipeline.staged_shard_builders,
            pipeline.staged_merge_workers,
            pipeline.page_prep_workers,
            default_cache_cap_for_available(memory.available_bytes),
            pipeline.pipeline_max_message_bytes_in_flight,
        )
    }
}

/// GH #496: the memory the planner sizes from. These are the host figures
/// clamped to the tightest limit on the process's cgroup chain (systemd
/// `MemoryMax`, container limits), the same probes the indexer's own defaults
/// use. Reading host `/proc/meminfo` here made `status` report host RAM under
/// a cap, and sized the planner's cache and in-flight caps for memory the
/// process may not use.
fn effective_memory_snapshot() -> MemorySnapshot {
    MemorySnapshot {
        total_bytes: crate::indexer::responsiveness::total_memory_bytes(),
        available_bytes: crate::indexer::responsiveness::available_memory_bytes(),
    }
}

pub(crate) fn inspect_host_topology_budget() -> TopologyBudgetPlan {
    inspect_host_topology_budget_with(effective_memory_snapshot())
}

fn inspect_host_topology_budget_with(memory: MemorySnapshot) -> TopologyBudgetPlan {
    let defaults = TopologyPlannerDefaults::from_current_process_with(memory);
    #[cfg(target_os = "linux")]
    {
        topology_budget_for_sysfs(Path::new("/sys"), memory, defaults)
    }
    #[cfg(not(target_os = "linux"))]
    {
        fallback_plan(
            fallback_topology(Some(memory), defaults.available_parallelism),
            defaults,
            "linux sysfs topology is unavailable on this platform".to_string(),
        )
    }
}

pub fn topology_budget_for_sysfs(
    sys_root: &Path,
    memory: MemorySnapshot,
    defaults: TopologyPlannerDefaults,
) -> TopologyBudgetPlan {
    match read_linux_sysfs_topology(sys_root, memory) {
        Ok(topology) => plan_for_topology(topology, defaults),
        Err(reason) => fallback_plan(
            fallback_topology(Some(memory), defaults.available_parallelism),
            defaults,
            reason,
        ),
    }
}

pub fn read_meminfo_snapshot(path: &Path) -> Option<MemorySnapshot> {
    let contents = fs::read_to_string(path).ok()?;
    let mut total_bytes = None;
    let mut available_bytes = None;
    for line in contents.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            total_bytes = parse_meminfo_kib(rest);
        } else if let Some(rest) = line.strip_prefix("MemAvailable:") {
            available_bytes = parse_meminfo_kib(rest);
        }
    }
    Some(MemorySnapshot {
        total_bytes,
        available_bytes,
    })
}

fn parse_meminfo_kib(rest: &str) -> Option<u64> {
    rest.split_whitespace()
        .next()?
        .parse::<u64>()
        .ok()?
        .checked_mul(1024)
}

fn read_linux_sysfs_topology(
    sys_root: &Path,
    memory: MemorySnapshot,
) -> Result<TopologySnapshot, String> {
    let cpu_root = sys_root.join("devices/system/cpu");
    let online_cpus = read_online_cpus(&cpu_root)?;
    if online_cpus.is_empty() {
        return Err("linux sysfs reported no online CPUs".to_string());
    }

    let mut sockets = BTreeSet::new();
    let mut core_threads: BTreeMap<(i64, i64), usize> = BTreeMap::new();
    let mut llc_group_keys = BTreeSet::new();

    for cpu in &online_cpus {
        let topology_dir = cpu_root.join(format!("cpu{cpu}/topology"));
        let package_id = read_i64(topology_dir.join("physical_package_id"))
            .map_err(|err| format!("missing package topology for cpu{cpu}: {err}"))?;
        let core_id = read_i64(topology_dir.join("core_id"))
            .map_err(|err| format!("missing core topology for cpu{cpu}: {err}"))?;
        sockets.insert(package_id);
        *core_threads.entry((package_id, core_id)).or_default() += 1;
        if let Some(group) =
            read_llc_group_key(&cpu_root.join(format!("cpu{cpu}/cache")), package_id)
        {
            llc_group_keys.insert(group);
        }
    }

    let physical_cores = core_threads.len().max(1);
    let smt_threads_per_core = core_threads.values().copied().max().unwrap_or(1).max(1);
    let sockets = sockets.len().max(1);
    let numa_nodes = read_numa_node_count(sys_root, &online_cpus)
        .unwrap_or(1)
        .max(1);
    let llc_groups = llc_group_keys.len().max(sockets);
    let logical_cpus = online_cpus.len();
    let topology_class = classify_topology(
        sockets,
        numa_nodes,
        physical_cores,
        logical_cpus,
        smt_threads_per_core,
    );

    Ok(TopologySnapshot {
        source: TopologySource::LinuxSysfs,
        topology_class,
        logical_cpus,
        physical_cores,
        sockets,
        numa_nodes,
        llc_groups,
        smt_threads_per_core,
        memory_total_bytes: memory.total_bytes,
        memory_available_bytes: memory.available_bytes,
    })
}

fn read_online_cpus(cpu_root: &Path) -> Result<BTreeSet<usize>, String> {
    let online_path = cpu_root.join("online");
    if let Ok(contents) = fs::read_to_string(&online_path) {
        return parse_cpu_list(&contents)
            .map_err(|err| format!("could not parse {}: {err}", online_path.display()));
    }

    let mut cpus = BTreeSet::new();
    let entries = fs::read_dir(cpu_root)
        .map_err(|err| format!("could not read {}: {err}", cpu_root.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(raw_id) = name.strip_prefix("cpu") else {
            continue;
        };
        if let Ok(cpu) = raw_id.parse::<usize>() {
            cpus.insert(cpu);
        }
    }
    if cpus.is_empty() {
        Err(format!(
            "no cpuN directories found under {}",
            cpu_root.display()
        ))
    } else {
        Ok(cpus)
    }
}

fn read_i64(path: PathBuf) -> Result<i64, String> {
    let raw = fs::read_to_string(&path).map_err(|err| err.to_string())?;
    raw.trim()
        .parse::<i64>()
        .map_err(|err| format!("{} is not an integer: {err}", path.display()))
}

fn read_llc_group_key(cache_root: &Path, package_id: i64) -> Option<String> {
    let entries = fs::read_dir(cache_root).ok()?;
    let mut fallback_id = None;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("index") {
            continue;
        }
        let index_dir = entry.path();
        let Ok(level) = fs::read_to_string(index_dir.join("level")) else {
            continue;
        };
        if level.trim() != "3" {
            continue;
        }
        if let Ok(cache_type) = fs::read_to_string(index_dir.join("type"))
            && cache_type.trim() != "Unified"
        {
            continue;
        }
        if let Ok(shared) = fs::read_to_string(index_dir.join("shared_cpu_list"))
            && let Ok(cpus) = parse_cpu_list(&shared)
        {
            return Some(format!("shared:{}", format_cpu_set(&cpus)));
        }
        if let Ok(id) = fs::read_to_string(index_dir.join("id")) {
            fallback_id = Some(format!("id:{package_id}:{}", id.trim()));
        }
    }
    fallback_id
}

fn read_numa_node_count(sys_root: &Path, online_cpus: &BTreeSet<usize>) -> Option<usize> {
    let node_root = sys_root.join("devices/system/node");
    let entries = fs::read_dir(node_root).ok()?;
    let mut count = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("node") {
            continue;
        }
        let cpulist = fs::read_to_string(entry.path().join("cpulist")).ok()?;
        let cpus = parse_cpu_list(&cpulist).ok()?;
        if cpus.iter().any(|cpu| online_cpus.contains(cpu)) {
            count += 1;
        }
    }
    (count > 0).then_some(count)
}

fn parse_cpu_list(contents: &str) -> Result<BTreeSet<usize>, String> {
    let mut cpus = BTreeSet::new();
    for part in contents.trim().split(',').filter(|part| !part.is_empty()) {
        let part = part.trim();
        if let Some((start, end)) = part.split_once('-') {
            let start = start
                .trim()
                .parse::<usize>()
                .map_err(|err| format!("invalid cpu-list start {start:?}: {err}"))?;
            let end = end
                .trim()
                .parse::<usize>()
                .map_err(|err| format!("invalid cpu-list end {end:?}: {err}"))?;
            if start > end {
                return Err(format!("invalid descending cpu range {start}-{end}"));
            }
            cpus.extend(start..=end);
        } else {
            cpus.insert(
                part.parse::<usize>()
                    .map_err(|err| format!("invalid cpu-list entry {part:?}: {err}"))?,
            );
        }
    }
    if cpus.is_empty() {
        Err("cpu list is empty".to_string())
    } else {
        Ok(cpus)
    }
}

fn format_cpu_set(cpus: &BTreeSet<usize>) -> String {
    cpus.iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn classify_topology(
    sockets: usize,
    numa_nodes: usize,
    physical_cores: usize,
    logical_cpus: usize,
    smt_threads_per_core: usize,
) -> TopologyClass {
    if sockets > 1 || numa_nodes > 1 {
        TopologyClass::MultiSocketNuma
    } else if physical_cores >= 32 || logical_cpus >= 64 {
        TopologyClass::ManyCoreSingleSocket
    } else if smt_threads_per_core > 1 {
        TopologyClass::SingleSocketSmt
    } else {
        TopologyClass::SingleSocket
    }
}

fn plan_for_topology(
    topology: TopologySnapshot,
    defaults: TopologyPlannerDefaults,
) -> TopologyBudgetPlan {
    let reserved_core_policy = reserved_core_policy_for(&topology, defaults.reserved_cores);
    let usable_logical = topology
        .logical_cpus
        .saturating_sub(reserved_core_policy.reserved_cores)
        .max(1);
    let physical_budget = topology.physical_cores.min(usable_logical).max(1);
    let locality_groups = topology.numa_nodes.max(topology.llc_groups).max(1);

    let shard_target = if physical_budget >= 64 {
        physical_budget / 2
    } else if physical_budget >= 32 {
        physical_budget * 3 / 8
    } else {
        physical_budget / 3
    }
    .max(1);
    let shard_builders = shard_target
        .max(defaults.shard_builders)
        .min(usable_logical)
        .clamp(1, 32);

    let merge_cap = usable_logical.div_ceil(4).clamp(1, 16);
    let merge_workers = locality_groups
        .saturating_mul(2)
        .max(defaults.merge_workers)
        .min(merge_cap)
        .max(1);

    let page_prep_workers = (physical_budget / 4)
        .max(defaults.page_prep_workers)
        .min(usable_logical)
        .clamp(1, 16);

    let semantic_divisor = if topology.smt_threads_per_core > 1 {
        8
    } else {
        6
    };
    let semantic_batchers = physical_budget
        .div_ceil(semantic_divisor)
        .max(topology.numa_nodes)
        .min(usable_logical)
        .clamp(1, 16);

    let cache_cap_bytes =
        topology_cache_cap(defaults.cache_cap_bytes, topology.memory_available_bytes);
    let max_inflight_bytes =
        topology_max_inflight_bytes(defaults.max_inflight_bytes, topology.memory_available_bytes);

    TopologyBudgetPlan {
        schema_version: TOPOLOGY_BUDGET_SCHEMA_VERSION.to_string(),
        fallback_active: false,
        decision_reason: format!(
            "planned from {:?}: {} logical CPUs, {} physical cores, {} socket(s), {} NUMA node(s), {} LLC group(s)",
            topology.topology_class,
            topology.logical_cpus,
            topology.physical_cores,
            topology.sockets,
            topology.numa_nodes,
            topology.llc_groups
        ),
        proof_notes: vec![
            "advisory only: live controllers keep current conservative settings until explicitly wired".to_string(),
            "CPU budgets prefer physical cores and LLC/NUMA locality over SMT oversubscription".to_string(),
            "RAM caps scale only when MemAvailable is large enough to preserve broad host headroom".to_string(),
        ],
        topology,
        reserved_core_policy,
        advisory_budgets: TopologyAdvisoryBudgets {
            shard_builders,
            merge_workers,
            page_prep_workers,
            semantic_batchers,
            cache_cap_bytes,
            max_inflight_bytes,
        },
        current_defaults: defaults,
    }
}

fn reserved_core_policy_for(
    topology: &TopologySnapshot,
    default_reserved_cores: usize,
) -> ReservedCorePolicy {
    let logical_cpus = topology.logical_cpus.max(1);
    let locality_groups = topology.numa_nodes.max(topology.llc_groups).max(1);
    let locality_reserve = if logical_cpus >= 64 {
        locality_groups.saturating_mul(2)
    } else {
        locality_groups
    };
    let smt_reserve = if topology.smt_threads_per_core > 1 && logical_cpus >= 16 {
        topology.smt_threads_per_core
    } else {
        0
    };
    let manycore_reserve = if logical_cpus >= 64 {
        logical_cpus / 12
    } else {
        0
    };
    let reserved_cores = default_reserved_cores
        .max(locality_reserve)
        .max(smt_reserve)
        .max(manycore_reserve)
        .min(16)
        .min(logical_cpus.saturating_sub(1));

    ReservedCorePolicy {
        reserved_cores,
        policy: "max(default, locality*2_on_large_hosts, smt_width, logical/12) capped at 16"
            .to_string(),
        reason: format!(
            "reserve {} of {} logical CPUs for interactive work, IO, and NUMA/LLC service headroom",
            reserved_cores, logical_cpus
        ),
    }
}

fn topology_cache_cap(default_cache_cap_bytes: usize, available_bytes: Option<u64>) -> usize {
    let Some(available_bytes) = available_bytes else {
        return default_cache_cap_bytes;
    };
    if available_bytes < 128 * GIB {
        return default_cache_cap_bytes;
    }
    let candidate = (available_bytes / 64).clamp(
        default_cache_cap_bytes as u64,
        TOPOLOGY_CACHE_BYTE_CAP_CEILING,
    );
    usize::try_from(candidate).unwrap_or(usize::MAX)
}

fn topology_max_inflight_bytes(
    default_max_inflight_bytes: usize,
    available_bytes: Option<u64>,
) -> usize {
    let Some(available_bytes) = available_bytes else {
        return default_max_inflight_bytes;
    };
    let candidate = (available_bytes / 4096).clamp(
        DEFAULT_MAX_INFLIGHT_FALLBACK as u64,
        TOPOLOGY_MAX_INFLIGHT_CEILING,
    );
    usize::try_from(candidate)
        .unwrap_or(usize::MAX)
        .max(default_max_inflight_bytes)
}

fn default_cache_cap_for_available(available_bytes: Option<u64>) -> usize {
    let Some(available_bytes) = available_bytes else {
        return DEFAULT_CACHE_BYTE_CAP_FALLBACK;
    };
    let ceiling = usize::try_from(DEFAULT_CACHE_BYTE_CAP_CEILING).unwrap_or(usize::MAX);
    let budget = available_bytes / DEFAULT_CACHE_BYTE_CAP_MEMORY_FRACTION_DENOMINATOR;
    let budget = budget.min(DEFAULT_CACHE_BYTE_CAP_CEILING);
    let budget = usize::try_from(budget).unwrap_or(ceiling);
    budget.clamp(DEFAULT_CACHE_BYTE_CAP_FALLBACK, ceiling)
}

fn fallback_topology(
    memory: Option<MemorySnapshot>,
    available_parallelism: usize,
) -> TopologySnapshot {
    let memory = memory.unwrap_or(MemorySnapshot {
        total_bytes: None,
        available_bytes: None,
    });
    TopologySnapshot {
        source: TopologySource::Fallback,
        topology_class: TopologyClass::Unknown,
        logical_cpus: available_parallelism.max(1),
        physical_cores: available_parallelism.max(1),
        sockets: 1,
        numa_nodes: 1,
        llc_groups: 1,
        smt_threads_per_core: 1,
        memory_total_bytes: memory.total_bytes,
        memory_available_bytes: memory.available_bytes,
    }
}

fn fallback_plan(
    topology: TopologySnapshot,
    defaults: TopologyPlannerDefaults,
    reason: String,
) -> TopologyBudgetPlan {
    let reserved_core_policy = ReservedCorePolicy {
        reserved_cores: defaults.reserved_cores,
        policy: "current conservative default".to_string(),
        reason: "topology could not be derived, so cass preserves existing worker and RAM defaults"
            .to_string(),
    };
    TopologyBudgetPlan {
        schema_version: TOPOLOGY_BUDGET_SCHEMA_VERSION.to_string(),
        topology,
        reserved_core_policy,
        advisory_budgets: TopologyAdvisoryBudgets {
            shard_builders: defaults.shard_builders,
            merge_workers: defaults.merge_workers,
            page_prep_workers: defaults.page_prep_workers,
            semantic_batchers: 1,
            cache_cap_bytes: defaults.cache_cap_bytes,
            max_inflight_bytes: defaults.max_inflight_bytes,
        },
        current_defaults: defaults,
        fallback_active: true,
        decision_reason: format!("using conservative defaults: {reason}"),
        proof_notes: vec![
            "fallback is intentionally isomorphic to current defaults for live rebuild budgets"
                .to_string(),
            "no /sys-derived CPU locality assumptions are made in fallback mode".to_string(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const GIB: u64 = 1024 * 1024 * 1024;

    fn defaults(cpus: usize) -> TopologyPlannerDefaults {
        TopologyPlannerDefaults::conservative(
            cpus,
            (cpus / 8).clamp(1, 8).min(cpus.saturating_sub(1)),
            8.min(cpus.max(1)),
            3,
            6.min(cpus.max(1)),
            2 * GIB as usize,
            32 * 1024 * 1024,
        )
    }

    fn memory(total_gib: u64, available_gib: u64) -> MemorySnapshot {
        MemorySnapshot {
            total_bytes: Some(total_gib * GIB),
            available_bytes: Some(available_gib * GIB),
        }
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, contents).expect("write fixture");
    }

    fn add_cpu(
        sys: &Path,
        cpu: usize,
        package_id: i64,
        core_id: i64,
        llc_id: usize,
        shared_cpu_list: &str,
    ) {
        let cpu_root = sys.join(format!("devices/system/cpu/cpu{cpu}"));
        write(
            &cpu_root.join("topology/physical_package_id"),
            &package_id.to_string(),
        );
        write(&cpu_root.join("topology/core_id"), &core_id.to_string());
        write(&cpu_root.join("cache/index3/level"), "3\n");
        write(&cpu_root.join("cache/index3/type"), "Unified\n");
        write(&cpu_root.join("cache/index3/id"), &llc_id.to_string());
        write(
            &cpu_root.join("cache/index3/shared_cpu_list"),
            shared_cpu_list,
        );
    }

    fn add_cpu_without_shared_llc(
        sys: &Path,
        cpu: usize,
        package_id: i64,
        core_id: i64,
        llc_id: usize,
    ) {
        let cpu_root = sys.join(format!("devices/system/cpu/cpu{cpu}"));
        write(
            &cpu_root.join("topology/physical_package_id"),
            &package_id.to_string(),
        );
        write(&cpu_root.join("topology/core_id"), &core_id.to_string());
        write(&cpu_root.join("cache/index3/level"), "3\n");
        write(&cpu_root.join("cache/index3/type"), "Unified\n");
        write(&cpu_root.join("cache/index3/id"), &llc_id.to_string());
    }

    #[test]
    fn one_socket_fixture_reports_single_socket_budget() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sys = temp.path();
        write(&sys.join("devices/system/cpu/online"), "0-7\n");
        for cpu in 0..8 {
            add_cpu(sys, cpu, 0, cpu as i64, 0, "0-7\n");
        }
        write(&sys.join("devices/system/node/node0/cpulist"), "0-7\n");

        let plan = topology_budget_for_sysfs(sys, memory(64, 48), defaults(8));

        assert!(!plan.fallback_active);
        assert_eq!(plan.topology.topology_class, TopologyClass::SingleSocket);
        assert_eq!(plan.topology.logical_cpus, 8);
        assert_eq!(plan.topology.physical_cores, 8);
        assert_eq!(plan.topology.sockets, 1);
        assert_eq!(plan.topology.numa_nodes, 1);
        assert_eq!(plan.topology.llc_groups, 1);
        assert_eq!(plan.topology.smt_threads_per_core, 1);
        assert!(plan.advisory_budgets.shard_builders > 0);
        assert!(
            plan.advisory_budgets.shard_builders
                <= plan
                    .topology
                    .logical_cpus
                    .saturating_sub(plan.reserved_core_policy.reserved_cores)
                    .max(1)
        );
    }

    #[test]
    fn smt_fixture_reports_threads_per_core() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sys = temp.path();
        write(&sys.join("devices/system/cpu/online"), "0-7\n");
        for cpu in 0..8 {
            add_cpu(sys, cpu, 0, (cpu % 4) as i64, 0, "0-7\n");
        }
        write(&sys.join("devices/system/node/node0/cpulist"), "0-7\n");

        let plan = topology_budget_for_sysfs(sys, memory(64, 48), defaults(8));

        assert!(!plan.fallback_active);
        assert_eq!(plan.topology.topology_class, TopologyClass::SingleSocketSmt);
        assert_eq!(plan.topology.logical_cpus, 8);
        assert_eq!(plan.topology.physical_cores, 4);
        assert_eq!(plan.topology.smt_threads_per_core, 2);
    }

    #[test]
    fn two_socket_numa_fixture_expands_locality_aware_budgets() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sys = temp.path();
        write(&sys.join("devices/system/cpu/online"), "0-63\n");
        for cpu in 0..64 {
            let socket = if cpu < 32 { 0 } else { 1 };
            let shared = if cpu < 32 { "0-31\n" } else { "32-63\n" };
            add_cpu(sys, cpu, socket, (cpu % 32) as i64, socket as usize, shared);
        }
        write(&sys.join("devices/system/node/node0/cpulist"), "0-31\n");
        write(&sys.join("devices/system/node/node1/cpulist"), "32-63\n");

        let plan = topology_budget_for_sysfs(sys, memory(256, 224), defaults(64));

        assert!(!plan.fallback_active);
        assert_eq!(plan.topology.topology_class, TopologyClass::MultiSocketNuma);
        assert_eq!(plan.topology.logical_cpus, 64);
        assert_eq!(plan.topology.physical_cores, 64);
        assert_eq!(plan.topology.sockets, 2);
        assert_eq!(plan.topology.numa_nodes, 2);
        assert_eq!(plan.topology.llc_groups, 2);
        assert_eq!(plan.reserved_core_policy.reserved_cores, 8);
        assert_eq!(plan.advisory_budgets.shard_builders, 21);
        assert_eq!(plan.advisory_budgets.merge_workers, 4);
        assert_eq!(plan.advisory_budgets.page_prep_workers, 14);
        assert_eq!(plan.advisory_budgets.semantic_batchers, 10);
        assert!(plan.advisory_budgets.cache_cap_bytes > plan.current_defaults.cache_cap_bytes);
    }

    #[test]
    fn llc_id_fallback_is_package_scoped_when_shared_cpu_list_is_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sys = temp.path();
        write(&sys.join("devices/system/cpu/online"), "0-3\n");
        add_cpu_without_shared_llc(sys, 0, 0, 0, 0);
        add_cpu_without_shared_llc(sys, 1, 0, 1, 0);
        add_cpu_without_shared_llc(sys, 2, 1, 0, 0);
        add_cpu_without_shared_llc(sys, 3, 1, 1, 0);
        write(&sys.join("devices/system/node/node0/cpulist"), "0-1\n");
        write(&sys.join("devices/system/node/node1/cpulist"), "2-3\n");

        let plan = topology_budget_for_sysfs(sys, memory(64, 48), defaults(4));

        assert!(!plan.fallback_active);
        assert_eq!(plan.topology.sockets, 2);
        assert_eq!(plan.topology.llc_groups, 2);
    }

    #[test]
    fn missing_topology_preserves_conservative_defaults() {
        let temp = tempfile::tempdir().expect("tempdir");
        let plan = topology_budget_for_sysfs(temp.path(), memory(256, 224), defaults(64));

        assert!(plan.fallback_active);
        assert_eq!(plan.topology.source, TopologySource::Fallback);
        assert_eq!(plan.topology.topology_class, TopologyClass::Unknown);
        assert_eq!(
            plan.advisory_budgets.shard_builders,
            plan.current_defaults.shard_builders
        );
        assert_eq!(
            plan.advisory_budgets.merge_workers,
            plan.current_defaults.merge_workers
        );
        assert_eq!(
            plan.advisory_budgets.page_prep_workers,
            plan.current_defaults.page_prep_workers
        );
        assert_eq!(
            plan.advisory_budgets.cache_cap_bytes,
            plan.current_defaults.cache_cap_bytes
        );
    }

    #[test]
    fn host_topology_budget_sizes_from_the_memory_it_is_given() {
        // GH #496: production hands the host planner the cgroup-clamped
        // probes. The planner must report and size from that figure, not
        // re-read host /proc/meminfo: under a 16 GiB cap on a big host,
        // status showed host RAM and the caches were sized for it.
        let capped = inspect_host_topology_budget_with(memory(16, 12));
        let roomy = inspect_host_topology_budget_with(memory(256, 224));
        assert_eq!(capped.topology.memory_total_bytes, Some(16 * GIB));
        assert_eq!(capped.topology.memory_available_bytes, Some(12 * GIB));
        assert_eq!(roomy.topology.memory_total_bytes, Some(256 * GIB));
        assert_eq!(
            capped.current_defaults.cache_cap_bytes,
            default_cache_cap_for_available(Some(12 * GIB))
        );
        assert!(
            capped.advisory_budgets.cache_cap_bytes <= roomy.advisory_budgets.cache_cap_bytes,
            "a capped process must not get a larger cache than a roomy one: {} > {}",
            capped.advisory_budgets.cache_cap_bytes,
            roomy.advisory_budgets.cache_cap_bytes
        );
    }

    #[test]
    fn meminfo_parser_reads_total_and_available_kib() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("meminfo");
        write(
            &path,
            "MemTotal:       268435456 kB\nMemAvailable:   234881024 kB\n",
        );

        let snapshot = read_meminfo_snapshot(&path).expect("meminfo snapshot");

        assert_eq!(snapshot.total_bytes, Some(256 * GIB));
        assert_eq!(snapshot.available_bytes, Some(224 * GIB));
    }
}
