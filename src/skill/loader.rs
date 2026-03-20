use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::path::Path;

use super::types::{LoadedSkill, SkillConfig};

/// Load a skill by name from the skills directory.
pub fn load_skill(skills_dir: &Path, skill_name: &str) -> Result<LoadedSkill> {
    let skill_dir = skills_dir.join(skill_name);
    if !skill_dir.exists() {
        return Err(anyhow!("Skill '{}' not found at {:?}", skill_name, skill_dir));
    }

    let skill_md_path = skill_dir.join("SKILL.md");
    let content = std::fs::read_to_string(&skill_md_path)
        .with_context(|| format!("Failed to read SKILL.md at {:?}", skill_md_path))?;

    let (config, body) = parse_skill_md(&content)
        .with_context(|| format!("Failed to parse SKILL.md for skill '{}'", skill_name))?;

    validate_skill_files(&skill_dir, &config)
        .with_context(|| format!("Skill '{}' references missing files", skill_name))?;

    let agent_prompts = load_agent_prompts(&skill_dir, &config)
        .with_context(|| format!("Failed to load agent prompts for skill '{}'", skill_name))?;

    Ok(LoadedSkill {
        config,
        body,
        skill_dir,
        agent_prompts,
    })
}

/// Split YAML frontmatter from markdown body.
/// Frontmatter is delimited by `---` on its own line at the start.
pub fn parse_skill_md(content: &str) -> Result<(SkillConfig, String)> {
    let content = content.trim_start();

    if !content.starts_with("---") {
        return Err(anyhow!("SKILL.md missing YAML frontmatter (expected leading '---')"));
    }

    // Find the closing ---
    let after_open = &content[3..];
    let close_pos = after_open
        .find("\n---")
        .ok_or_else(|| anyhow!("SKILL.md frontmatter is not closed with '---'"))?;

    let frontmatter = &after_open[..close_pos].trim_start_matches('\n');
    // body starts after closing --- and optional newline
    let body_start = 3 + close_pos + 4; // len("---") + close_pos + len("\n---")
    let body = if body_start < content.len() {
        content[body_start..].trim_start_matches('\n').to_string()
    } else {
        String::new()
    };

    let config: SkillConfig = serde_yaml::from_str(frontmatter)
        .with_context(|| "Failed to deserialize YAML frontmatter")?;

    Ok((config, body))
}

/// Validate that all files referenced in the skill config exist.
pub fn validate_skill_files(skill_dir: &Path, config: &SkillConfig) -> Result<()> {
    for monitor in &config.monitors {
        let script_path = skill_dir.join("scripts").join(&monitor.script);
        if !script_path.exists() {
            return Err(anyhow!(
                "Monitor '{}' references missing script: {:?}",
                monitor.name,
                script_path
            ));
        }
    }

    for action in &config.actions {
        let script_path = skill_dir.join("scripts").join(&action.script);
        if !script_path.exists() {
            return Err(anyhow!(
                "Action '{}' references missing script: {:?}",
                action.name,
                script_path
            ));
        }
    }

    for (agent_name, agent_def) in &config.agents {
        let prompt_path = skill_dir.join("prompts").join(&agent_def.system_prompt_file);
        if !prompt_path.exists() {
            return Err(anyhow!(
                "Agent '{}' references missing prompt file: {:?}",
                agent_name,
                prompt_path
            ));
        }
    }

    Ok(())
}

/// Load all agent prompt files referenced in the config.
pub fn load_agent_prompts(
    skill_dir: &Path,
    config: &SkillConfig,
) -> Result<HashMap<String, String>> {
    let mut prompts = HashMap::new();

    for (agent_name, agent_def) in &config.agents {
        let prompt_path = skill_dir.join("prompts").join(&agent_def.system_prompt_file);
        let content = std::fs::read_to_string(&prompt_path)
            .with_context(|| format!("Failed to read prompt file {:?}", prompt_path))?;
        prompts.insert(agent_name.clone(), content);
    }

    Ok(prompts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Build a minimal skill directory and return the TempDir (keep alive).
    fn create_minimal_skill(base: &Path, skill_name: &str) -> std::path::PathBuf {
        let skill_dir = base.join(skill_name);
        let scripts_dir = skill_dir.join("scripts");
        let prompts_dir = skill_dir.join("prompts");
        std::fs::create_dir_all(&scripts_dir).unwrap();
        std::fs::create_dir_all(&prompts_dir).unwrap();

        let skill_md = r#"---
name: test-skill
description: A test skill
monitors:
  - name: check-health
    interval: 30s
    script: health.sh
    trigger_when: "exit_code != 0"
actions:
  - name: restart
    risk: low
    script: restart.sh
agents:
  responder:
    system_prompt_file: responder.md
---
# Test Skill

This is the skill body.
"#;
        write_file(&skill_dir.join("SKILL.md"), skill_md);
        write_file(&scripts_dir.join("health.sh"), "#!/bin/bash\nexit 0");
        write_file(&scripts_dir.join("restart.sh"), "#!/bin/bash\necho restarting");
        write_file(&prompts_dir.join("responder.md"), "You are a responder agent.");

        skill_dir
    }

    #[test]
    fn test_load_skill_success() {
        let tmp = TempDir::new().unwrap();
        create_minimal_skill(tmp.path(), "test-skill");

        let loaded = load_skill(tmp.path(), "test-skill").expect("load_skill should succeed");

        assert_eq!(loaded.config.name, "test-skill");
        assert_eq!(loaded.config.monitors.len(), 1);
        assert_eq!(loaded.config.monitors[0].name, "check-health");
        assert_eq!(loaded.config.actions.len(), 1);
        assert_eq!(loaded.config.actions[0].name, "restart");
        assert!(loaded.agent_prompts.contains_key("responder"));
        assert!(loaded.agent_prompts["responder"].contains("responder agent"));
        assert!(loaded.body.contains("Test Skill"));
        assert_eq!(loaded.skill_dir, tmp.path().join("test-skill"));
    }

    #[test]
    fn test_load_skill_missing() {
        let tmp = TempDir::new().unwrap();
        let result = load_skill(tmp.path(), "nonexistent-skill");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("nonexistent-skill"), "error should mention skill name: {}", err);
    }

    #[test]
    fn test_parse_frontmatter() {
        let content = r#"---
name: my-skill
description: Does things
monitors:
  - name: ping
    interval: 10s
    script: ping.sh
    trigger_when: "exit_code != 0"
---
# Body here
"#;
        let (config, body) = parse_skill_md(content).expect("should parse");
        assert_eq!(config.name, "my-skill");
        assert_eq!(config.description.as_deref(), Some("Does things"));
        assert_eq!(config.monitors.len(), 1);
        assert_eq!(config.monitors[0].parse, "exit-code"); // default
        assert!(body.contains("Body here"));
    }
}
