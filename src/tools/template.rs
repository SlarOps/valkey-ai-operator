use anyhow::{anyhow, Result};
use regex::Regex;
use std::collections::HashMap;

/// Render a template string by replacing ${VAR} and ${VAR:default} placeholders
pub fn render_template(template: &str, vars: &HashMap<String, String>) -> Result<String> {
    let re = Regex::new(r"\$\{([^}]+)\}").unwrap();
    let mut missing = Vec::new();

    let result = re.replace_all(template, |caps: &regex::Captures| {
        let expr = &caps[1];
        if let Some((var_name, default_val)) = expr.split_once(':') {
            vars.get(var_name)
                .cloned()
                .unwrap_or_else(|| default_val.to_string())
        } else {
            match vars.get(expr) {
                Some(val) => val.clone(),
                None => {
                    missing.push(expr.to_string());
                    caps[0].to_string()
                }
            }
        }
    });

    if !missing.is_empty() {
        return Err(anyhow!("Missing template variables: {:?}", missing));
    }

    Ok(result.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_replacement() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "my-app".to_string());
        vars.insert("replicas".to_string(), "3".to_string());
        let result = render_template("name: ${name}\nreplicas: ${replicas}", &vars).unwrap();
        assert_eq!(result, "name: my-app\nreplicas: 3");
    }

    #[test]
    fn test_default_value() {
        let vars = HashMap::new();
        let result = render_template("port: ${port:6379}", &vars).unwrap();
        assert_eq!(result, "port: 6379");
    }

    #[test]
    fn test_missing_variable() {
        let vars = HashMap::new();
        let result = render_template("name: ${name}", &vars);
        assert!(result.is_err());
    }

    #[test]
    fn test_mixed() {
        let mut vars = HashMap::new();
        vars.insert("name".to_string(), "my-app".to_string());
        let result = render_template("${name}:${port:8080}", &vars).unwrap();
        assert_eq!(result, "my-app:8080");
    }

    #[test]
    fn test_no_placeholders() {
        let vars = HashMap::new();
        let result = render_template("plain text", &vars).unwrap();
        assert_eq!(result, "plain text");
    }

    #[test]
    fn test_multiple_defaults() {
        let vars = HashMap::new();
        let result =
            render_template("${host:localhost}:${port:5432}", &vars).unwrap();
        assert_eq!(result, "localhost:5432");
    }
}
