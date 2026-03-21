use crate::skill::types::{LoadedSkill, MonitorDef};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct MonitorInstance {
    pub resource_namespace: String,
    pub resource_name: String,
    pub monitor_def: MonitorDef,
    pub skill_dir: std::path::PathBuf,
    pub interval: Duration,
    pub last_run: Option<Instant>,
    pub last_output: Option<Value>,
}

pub struct MonitorRegistry {
    monitors: HashMap<String, Vec<MonitorInstance>>,
}

impl MonitorRegistry {
    pub fn new() -> Self {
        Self { monitors: HashMap::new() }
    }

    pub fn register(&mut self, namespace: &str, name: &str, skill: &LoadedSkill) {
        let key = format!("{}/{}", namespace, name);
        let instances: Vec<MonitorInstance> = skill.config.monitors.iter().map(|m| {
            MonitorInstance {
                resource_namespace: namespace.to_string(),
                resource_name: name.to_string(),
                monitor_def: m.clone(),
                skill_dir: skill.skill_dir.clone(),
                interval: parse_duration(&m.interval).unwrap_or(Duration::from_secs(30)),
                last_run: None,
                last_output: None,
            }
        }).collect();
        self.monitors.insert(key, instances);
    }

    pub fn unregister(&mut self, namespace: &str, name: &str) {
        let key = format!("{}/{}", namespace, name);
        self.monitors.remove(&key);
    }

    pub fn due_monitors(&mut self) -> Vec<&mut MonitorInstance> {
        let now = Instant::now();
        self.monitors.values_mut()
            .flat_map(|instances| instances.iter_mut())
            .filter(|m| match m.last_run {
                None => true,
                Some(last) => now.duration_since(last) >= m.interval,
            })
            .collect()
    }

    pub fn get_state(&self, namespace: &str, name: &str) -> HashMap<String, Value> {
        let key = format!("{}/{}", namespace, name);
        let mut state = HashMap::new();
        if let Some(instances) = self.monitors.get(&key) {
            for m in instances {
                if let Some(output) = &m.last_output {
                    state.insert(m.monitor_def.name.clone(), output.clone());
                }
            }
        }
        state
    }

    pub fn monitors_mut(&mut self, namespace: &str, name: &str) -> Option<&mut Vec<MonitorInstance>> {
        let key = format!("{}/{}", namespace, name);
        self.monitors.get_mut(&key)
    }

    pub fn has_monitors(&self, namespace: &str, name: &str) -> bool {
        let key = format!("{}/{}", namespace, name);
        self.monitors.get(&key).map_or(false, |m| !m.is_empty())
    }
}

fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if let Some(secs) = s.strip_suffix('s') {
        secs.parse::<u64>().ok().map(Duration::from_secs)
    } else if let Some(mins) = s.strip_suffix('m') {
        mins.parse::<u64>().ok().map(|m| Duration::from_secs(m * 60))
    } else {
        s.parse::<u64>().ok().map(Duration::from_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::types::*;
    use std::path::PathBuf;

    fn test_skill() -> LoadedSkill {
        LoadedSkill {
            config: SkillConfig {
                name: "test".to_string(),
                description: None,
                allowed_tools: None,
                monitors: vec![MonitorDef {
                    name: "health".to_string(),
                    interval: "10s".to_string(),
                    script: "scripts/monitors/health.sh".to_string(),
                    parse: "exit-code".to_string(),
                    trigger_when: "exit_code != 0".to_string(),
                }],
                actions: vec![],
                agents: HashMap::new(),
            },
            body: String::new(),
            skill_dir: PathBuf::from("/skills/test"),
            agent_prompts: HashMap::new(),
        }
    }

    #[test]
    fn test_register_and_due() {
        let mut registry = MonitorRegistry::new();
        registry.register("default", "my-app", &test_skill());
        let due = registry.due_monitors();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].monitor_def.name, "health");
    }

    #[test]
    fn test_unregister() {
        let mut registry = MonitorRegistry::new();
        registry.register("default", "my-app", &test_skill());
        registry.unregister("default", "my-app");
        assert!(!registry.has_monitors("default", "my-app"));
    }

    #[test]
    fn test_parse_duration_variants() {
        assert_eq!(parse_duration("10s"), Some(Duration::from_secs(10)));
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("30"), Some(Duration::from_secs(30)));
    }
}
