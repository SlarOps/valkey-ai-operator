use serde_json::{json, Value};

/// Result of running a monitor
pub struct MonitorResult {
    pub resource_namespace: String,
    pub resource_name: String,
    pub monitor_name: String,
    pub output: Value,
    pub triggered: bool,
    pub previous_output: Option<Value>,
}

/// Parse monitor script output based on parse type
pub fn parse_monitor_output(parse_type: &str, stdout: &str, exit_code: i32) -> Value {
    match parse_type {
        "key-value" => {
            let mut map = serde_json::Map::new();
            for line in stdout.lines() {
                if let Some((key, value)) = line.split_once('=') {
                    map.insert(
                        key.trim().to_string(),
                        Value::String(value.trim().to_string()),
                    );
                }
            }
            Value::Object(map)
        }
        "json" => {
            serde_json::from_str(stdout)
                .unwrap_or_else(|_| json!({"raw": stdout, "exit_code": exit_code}))
        }
        "exit-code" | _ => {
            json!({"exit_code": exit_code})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_key_value() {
        let output = parse_monitor_output("key-value", "cluster_state=ok\nslots_ok=16384\n", 0);
        assert_eq!(output["cluster_state"], "ok");
        assert_eq!(output["slots_ok"], "16384");
    }

    #[test]
    fn test_parse_exit_code() {
        let output = parse_monitor_output("exit-code", "some output", 0);
        assert_eq!(output["exit_code"], 0);
    }

    #[test]
    fn test_parse_exit_code_failure() {
        let output = parse_monitor_output("exit-code", "", 1);
        assert_eq!(output["exit_code"], 1);
    }

    #[test]
    fn test_parse_json() {
        let output = parse_monitor_output("json", r#"{"status":"ok","lag":0}"#, 0);
        assert_eq!(output["status"], "ok");
        assert_eq!(output["lag"], 0);
    }

    #[test]
    fn test_parse_json_invalid() {
        let output = parse_monitor_output("json", "not json", 1);
        assert_eq!(output["exit_code"], 1);
    }
}
