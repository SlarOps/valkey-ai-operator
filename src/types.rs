use crate::crd::ValkeyClusterSpec;
use std::collections::HashMap;

/// Snapshot of actual cluster state, built by reconciler and sent to agent.
#[derive(Debug, Clone)]
pub struct StateSnapshot {
    pub cluster_name: String,
    pub namespace: String,
    pub spec: ValkeyClusterSpec,
    // K8s state
    pub sts_exists: bool,
    pub sts_replicas: Option<u32>,
    pub sts_memory_limit: Option<String>,
    pub sts_cpu_limit: Option<String>,
    pub pods_ready: u32,
    pub pods_total: u32,
    pub pods: Vec<PodInfo>,
    // Valkey cluster state (from CLUSTER INFO via exec)
    pub valkey_cluster_state: Option<String>,  // "ok", "fail", or None if can't reach
    pub valkey_cluster_slots_ok: Option<u32>,
    pub valkey_cluster_known_nodes: Option<u32>,
    pub valkey_cluster_size: Option<u32>,       // number of masters with slots
    // Trigger
    pub trigger: String,
}

#[derive(Debug, Clone)]
pub struct PodInfo {
    pub name: String,
    pub phase: String,
    pub ready: bool,
    pub restart_count: i32,
}

impl StateSnapshot {
    /// Build the context message for the agent.
    pub fn to_agent_message(&self) -> String {
        let mut msg = String::new();

        msg.push_str(&format!("## Cluster '{}' in namespace '{}'\n\n", self.cluster_name, self.namespace));
        msg.push_str(&format!("### Trigger: {}\n\n", self.trigger));

        // Desired
        msg.push_str("### Desired State (from CRD spec)\n");
        msg.push_str(&format!("- masters: {}\n", self.spec.masters));
        msg.push_str(&format!("- replicas_per_master: {}\n", self.spec.replicas_per_master));
        msg.push_str(&format!("- total_pods: {}\n", self.spec.total_pods()));
        msg.push_str(&format!("- image: {}\n", self.spec.image()));
        if let Some(ref res) = self.spec.resources.limits {
            msg.push_str(&format!("- memory_limit: {}\n", res.memory.as_deref().unwrap_or("not set")));
            msg.push_str(&format!("- cpu_limit: {}\n", res.cpu.as_deref().unwrap_or("not set")));
        }
        msg.push('\n');

        // Actual K8s
        msg.push_str("### Actual State (from K8s)\n");
        if self.sts_exists {
            msg.push_str(&format!("- statefulset: exists, replicas={}\n", self.sts_replicas.unwrap_or(0)));
            if let Some(ref mem) = self.sts_memory_limit {
                msg.push_str(&format!("- sts_memory_limit: {}\n", mem));
            }
            if let Some(ref cpu) = self.sts_cpu_limit {
                msg.push_str(&format!("- sts_cpu_limit: {}\n", cpu));
            }
        } else {
            msg.push_str("- statefulset: DOES NOT EXIST\n");
        }
        msg.push_str(&format!("- pods: {}/{} ready\n", self.pods_ready, self.pods_total));
        if !self.pods.is_empty() {
            for p in &self.pods {
                msg.push_str(&format!("  - {}: phase={}, ready={}, restarts={}\n",
                    p.name, p.phase, p.ready, p.restart_count));
            }
        }
        msg.push('\n');

        // Actual Valkey cluster
        msg.push_str("### Valkey Cluster State\n");
        match &self.valkey_cluster_state {
            Some(state) => {
                msg.push_str(&format!("- cluster_state: {}\n", state));
                msg.push_str(&format!("- slots_ok: {}/16384\n", self.valkey_cluster_slots_ok.unwrap_or(0)));
                msg.push_str(&format!("- known_nodes: {}\n", self.valkey_cluster_known_nodes.unwrap_or(0)));
                msg.push_str(&format!("- cluster_size (masters with slots): {}\n", self.valkey_cluster_size.unwrap_or(0)));
            }
            None => {
                msg.push_str("- UNREACHABLE (no pods or cluster not initialized)\n");
            }
        }
        msg.push('\n');

        // Diff / attention
        msg.push_str("### What Needs Attention\n");
        let mut issues = Vec::new();

        if !self.sts_exists {
            issues.push("- Cluster needs to be CREATED from scratch (no StatefulSet exists)".into());
        } else {
            // Replicas mismatch
            let desired = self.spec.total_pods();
            if let Some(actual) = self.sts_replicas {
                if actual != desired {
                    issues.push(format!("- StatefulSet replicas: actual={}, desired={}", actual, desired));
                }
            }
            // Resource mismatch
            if let Some(ref spec_limits) = self.spec.resources.limits {
                if let (Some(ref spec_mem), Some(ref sts_mem)) = (&spec_limits.memory, &self.sts_memory_limit) {
                    if spec_mem != sts_mem {
                        issues.push(format!("- Memory limit mismatch: sts={}, spec={}", sts_mem, spec_mem));
                    }
                }
                if let (Some(ref spec_cpu), Some(ref sts_cpu)) = (&spec_limits.cpu, &self.sts_cpu_limit) {
                    if spec_cpu != sts_cpu {
                        issues.push(format!("- CPU limit mismatch: sts={}, spec={}", sts_cpu, spec_cpu));
                    }
                }
            }
            // Pods not ready
            if self.pods_ready < self.pods_total {
                issues.push(format!("- Pods not ready: {}/{}", self.pods_ready, self.pods_total));
            }
            // Crash loops
            for p in &self.pods {
                if p.restart_count > 2 {
                    issues.push(format!("- Pod {} has {} restarts (possible crash loop)", p.name, p.restart_count));
                }
            }
            // Valkey cluster issues
            if let Some(ref state) = self.valkey_cluster_state {
                if state != "ok" {
                    issues.push(format!("- Valkey cluster_state: {} (NOT ok)", state));
                }
                if let Some(masters) = self.valkey_cluster_size {
                    if masters != self.spec.masters {
                        issues.push(format!("- Valkey masters: actual={}, desired={}", masters, self.spec.masters));
                    }
                }
            }
        }

        if issues.is_empty() {
            msg.push_str("- Everything looks healthy. Verify with cluster_info if needed.\n");
        } else {
            for issue in &issues {
                msg.push_str(issue);
                msg.push('\n');
            }
        }

        msg.push_str("\nDecide what to do and execute. Call update_cluster_status as your final action.\n");
        msg
    }
}

/// Circuit breaker: tracks consecutive failures per cluster.
pub struct CircuitBreaker {
    failures: HashMap<String, u32>,
    max_failures: u32,
}

impl CircuitBreaker {
    pub fn new(max_failures: u32) -> Self {
        Self { failures: HashMap::new(), max_failures }
    }

    pub fn is_open(&self, key: &str) -> bool {
        self.failures.get(key).map_or(false, |c| *c >= self.max_failures)
    }

    pub fn record_success(&mut self, key: &str) {
        self.failures.remove(key);
    }

    pub fn record_failure(&mut self, key: &str) -> u32 {
        let count = self.failures.entry(key.to_string()).or_insert(0);
        *count += 1;
        *count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_circuit_breaker() {
        let mut cb = CircuitBreaker::new(3);
        assert!(!cb.is_open("a"));
        cb.record_failure("a");
        cb.record_failure("a");
        assert!(!cb.is_open("a"));
        cb.record_failure("a");
        assert!(cb.is_open("a"));
        cb.record_success("a");
        assert!(!cb.is_open("a"));
    }
}
