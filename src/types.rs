use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Generic state snapshot built from monitors + K8s state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub resource: ResourceInfo,
    pub monitors: HashMap<String, Value>,
    pub k8s: K8sState,
    pub trigger: TriggerInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceInfo {
    pub name: String,
    pub namespace: String,
    pub uid: String,
    pub skill: String,
    pub goal: String,
    pub image: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct K8sState {
    #[serde(default)]
    pub pods: Vec<PodInfo>,
    #[serde(default)]
    pub statefulsets: Vec<StatefulSetInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodInfo {
    pub name: String,
    pub phase: String,
    pub ready: bool,
    pub restarts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatefulSetInfo {
    pub name: String,
    pub replicas: u32,
    pub ready_replicas: u32,
    #[serde(default)]
    pub memory_limit: Option<String>,
    #[serde(default)]
    pub cpu_limit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerInfo {
    pub source: String,
    pub reason: String,
    pub timestamp: String,
}

/// Info about detected drift in child resources
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftInfo {
    pub missing_resources: Vec<String>,
}

/// Events sent through per-resource channel
#[derive(Debug, Clone)]
pub enum ResourceEvent {
    Bootstrap(StateSnapshot),
    MonitorTrigger(StateSnapshot),
    SpecChange(StateSnapshot),
    DriftDetected(StateSnapshot, DriftInfo),
    Shutdown,
}

/// Circuit breaker for per-instance failure tracking
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    pub consecutive_failures: u32,
    pub consecutive_successes: u32,
    pub max_failures: u32,
    pub required_successes: u32,
    pub state: CircuitState,
    pub opened_at: Option<std::time::Instant>,
    pub reset_timeout_secs: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

impl CircuitBreaker {
    pub fn new(max_failures: u32, required_successes: u32) -> Self {
        Self {
            consecutive_failures: 0,
            consecutive_successes: 0,
            max_failures,
            required_successes,
            state: CircuitState::Closed,
            opened_at: None,
            reset_timeout_secs: 60,
        }
    }

    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.consecutive_successes += 1;
        if self.state == CircuitState::HalfOpen && self.consecutive_successes >= self.required_successes {
            self.state = CircuitState::Closed;
        }
    }

    pub fn record_failure(&mut self) {
        self.consecutive_successes = 0;
        self.consecutive_failures += 1;
        if self.consecutive_failures >= self.max_failures {
            self.state = CircuitState::Open;
            self.opened_at = Some(std::time::Instant::now());
        }
    }

    pub fn is_open(&mut self) -> bool {
        if self.state == CircuitState::Open {
            if let Some(opened) = self.opened_at {
                if opened.elapsed() >= std::time::Duration::from_secs(self.reset_timeout_secs) {
                    tracing::info!("Circuit breaker auto-resetting to HalfOpen after {}s", self.reset_timeout_secs);
                    self.state = CircuitState::HalfOpen;
                    self.opened_at = None;
                    self.consecutive_failures = 0;
                    self.consecutive_successes = 0;
                    return false;
                }
            }
            return true;
        }
        false
    }

    pub fn manual_reset(&mut self) {
        self.state = CircuitState::HalfOpen;
        self.opened_at = None;
        self.consecutive_failures = 0;
        self.consecutive_successes = 0;
    }
}

impl StateSnapshot {
    pub fn to_agent_message(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| format!("{:?}", self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_circuit_breaker_opens() {
        let mut cb = CircuitBreaker::new(3, 2);
        cb.record_failure();
        cb.record_failure();
        assert!(!cb.is_open());
        cb.record_failure();
        assert!(cb.is_open());
        assert!(cb.opened_at.is_some());
    }

    #[test]
    fn test_circuit_breaker_half_open() {
        let mut cb = CircuitBreaker::new(3, 2);
        cb.record_failure();
        cb.record_failure();
        cb.record_failure();
        assert!(cb.is_open());
        cb.manual_reset();
        assert_eq!(cb.state, CircuitState::HalfOpen);
        assert!(cb.opened_at.is_none());
        cb.record_success();
        assert_eq!(cb.state, CircuitState::HalfOpen);
        cb.record_success();
        assert_eq!(cb.state, CircuitState::Closed);
    }

    #[test]
    fn test_circuit_breaker_success_resets() {
        let mut cb = CircuitBreaker::new(3, 2);
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        assert_eq!(cb.consecutive_failures, 0);
        assert!(!cb.is_open());
    }

    #[test]
    fn test_circuit_breaker_auto_reset() {
        let mut cb = CircuitBreaker::new(3, 2);
        cb.reset_timeout_secs = 0; // instant reset for testing
        cb.record_failure();
        cb.record_failure();
        cb.record_failure();
        assert_eq!(cb.state, CircuitState::Open);
        // With 0s timeout, next is_open() check should auto-reset to HalfOpen
        assert!(!cb.is_open());
        assert_eq!(cb.state, CircuitState::HalfOpen);
    }
}
