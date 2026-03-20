use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Parsed from SKILL.md YAML frontmatter
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SkillConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default, rename = "allowed-tools")]
    pub allowed_tools: Option<String>,
    #[serde(default)]
    pub monitors: Vec<MonitorDef>,
    #[serde(default)]
    pub actions: Vec<ActionDef>,
    #[serde(default)]
    pub agents: HashMap<String, AgentDef>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MonitorDef {
    pub name: String,
    pub interval: String,
    pub script: String,
    #[serde(default = "default_parse")]
    pub parse: String,
    pub trigger_when: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ActionDef {
    pub name: String,
    pub risk: RiskLevel,
    #[serde(default)]
    pub description: Option<String>,
    pub script: String,
    #[serde(default)]
    pub params: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentDef {
    pub system_prompt_file: String,
}

/// Fully loaded skill with resolved paths and content
#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub config: SkillConfig,
    pub body: String,
    pub skill_dir: PathBuf,
    pub agent_prompts: HashMap<String, String>,
}

fn default_parse() -> String {
    "exit-code".to_string()
}
