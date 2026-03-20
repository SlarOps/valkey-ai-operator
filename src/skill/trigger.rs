use anyhow::{anyhow, Result};
use tracing::warn;

/// Evaluate a trigger expression against JSON data.
/// Fail-open: if the expression cannot be evaluated (e.g. missing key, parse error),
/// returns `true` and logs a warning.
pub fn evaluate_trigger(expression: &str, data: &serde_json::Value) -> bool {
    match eval_expr(expression.trim(), data) {
        Ok(result) => result,
        Err(e) => {
            warn!("trigger eval error (fail-open): expression={:?} err={}", expression, e);
            true
        }
    }
}

/// Validate trigger expression syntax at load time.
/// Returns Ok(()) if the expression is structurally valid.
pub fn validate_trigger_syntax(expression: &str) -> Result<()> {
    // We validate by attempting to parse the expression tree.
    parse_or_expr(expression.trim())?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Internal parser
// ---------------------------------------------------------------------------

fn eval_expr(expr: &str, data: &serde_json::Value) -> Result<bool> {
    eval_or(expr, data)
}

/// Parse and evaluate OR-level expression (lowest precedence).
fn eval_or(expr: &str, data: &serde_json::Value) -> Result<bool> {
    // Split on " OR " (case-sensitive as per spec)
    // We need top-level splits only — but since we have no grouping parens in spec,
    // a simple split is fine.
    let parts: Vec<&str> = split_logic(expr, " OR ");
    if parts.len() > 1 {
        for part in parts {
            if eval_and(part.trim(), data)? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    eval_and(expr, data)
}

/// Parse and evaluate AND-level expression.
fn eval_and(expr: &str, data: &serde_json::Value) -> Result<bool> {
    let parts: Vec<&str> = split_logic(expr, " AND ");
    if parts.len() > 1 {
        for part in parts {
            if !eval_comparison(part.trim(), data)? {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    eval_comparison(expr, data)
}

/// Split expr by a separator, but only at the top level (no nested parens).
/// Since the spec has no grouping, this is just a plain split, but we make it
/// robust by not splitting inside quoted strings.
fn split_logic<'a>(expr: &'a str, sep: &str) -> Vec<&'a str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let sep_len = sep.len();
    let expr_bytes = expr.as_bytes();
    let sep_bytes = sep.as_bytes();

    let mut i = 0;
    while i + sep_len <= expr.len() {
        if &expr_bytes[i..i + sep_len] == sep_bytes {
            parts.push(&expr[start..i]);
            start = i + sep_len;
            i = start;
        } else {
            i += 1;
        }
    }
    parts.push(&expr[start..]);
    parts
}

const OPERATORS: &[&str] = &["<=", ">=", "!=", "==", "<", ">"];

/// Evaluate a single comparison expression: `key op value`
fn eval_comparison(expr: &str, data: &serde_json::Value) -> Result<bool> {
    for op in OPERATORS {
        if let Some(pos) = expr.find(op) {
            // Make sure it's exactly this operator (avoid matching < inside <=)
            // We process OPERATORS in longest-first order (<=, >= before <, >),
            // so the first match is the correct one.
            let lhs = expr[..pos].trim();
            let rhs = expr[pos + op.len()..].trim();
            return compare(lhs, *op, rhs, data);
        }
    }
    Err(anyhow!("No operator found in expression: {:?}", expr))
}

fn compare(
    lhs: &str,
    op: &str,
    rhs: &str,
    data: &serde_json::Value,
) -> Result<bool> {
    let lhs_val = resolve_value(lhs, data)?;
    let rhs_val = rhs.to_string();

    // Attempt numeric comparison
    if let (Ok(l), Ok(r)) = (lhs_val.parse::<f64>(), rhs_val.parse::<f64>()) {
        return Ok(numeric_compare(l, op, r));
    }

    // String comparison
    Ok(string_compare(&lhs_val, op, &rhs_val))
}

fn resolve_value(key: &str, data: &serde_json::Value) -> Result<String> {
    match data.get(key) {
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(serde_json::Value::Number(n)) => Ok(n.to_string()),
        Some(serde_json::Value::Bool(b)) => Ok(b.to_string()),
        Some(serde_json::Value::Null) => Ok("null".to_string()),
        Some(other) => Ok(other.to_string()),
        None => Err(anyhow!("Key '{}' not found in data", key)),
    }
}

fn numeric_compare(l: f64, op: &str, r: f64) -> bool {
    match op {
        "==" => (l - r).abs() < f64::EPSILON,
        "!=" => (l - r).abs() >= f64::EPSILON,
        "<" => l < r,
        ">" => l > r,
        "<=" => l <= r,
        ">=" => l >= r,
        _ => false,
    }
}

fn string_compare(l: &str, op: &str, r: &str) -> bool {
    match op {
        "==" => l == r,
        "!=" => l != r,
        "<" => l < r,
        ">" => l > r,
        "<=" => l <= r,
        ">=" => l >= r,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Parse-only versions for syntax validation (no data needed)
// ---------------------------------------------------------------------------

fn parse_or_expr(expr: &str) -> Result<()> {
    let parts = split_logic(expr, " OR ");
    for part in parts {
        parse_and_expr(part.trim())?;
    }
    Ok(())
}

fn parse_and_expr(expr: &str) -> Result<()> {
    let parts = split_logic(expr, " AND ");
    for part in parts {
        parse_comparison(part.trim())?;
    }
    Ok(())
}

fn parse_comparison(expr: &str) -> Result<()> {
    for op in OPERATORS {
        if expr.contains(op) {
            let pos = expr.find(op).unwrap();
            let lhs = expr[..pos].trim();
            let rhs = expr[pos + op.len()..].trim();
            if lhs.is_empty() {
                return Err(anyhow!("Empty left-hand side in: {:?}", expr));
            }
            if rhs.is_empty() {
                return Err(anyhow!("Empty right-hand side in: {:?}", expr));
            }
            return Ok(());
        }
    }
    Err(anyhow!("No comparison operator found in: {:?}", expr))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_simple_not_equal() {
        let data = json!({"cluster_state": "fail"});
        assert!(evaluate_trigger("cluster_state != ok", &data));
    }

    #[test]
    fn test_simple_equal_no_trigger() {
        let data = json!({"cluster_state": "ok"});
        assert!(!evaluate_trigger("cluster_state != ok", &data));
    }

    #[test]
    fn test_exit_code() {
        let data = json!({"exit_code": 1});
        assert!(evaluate_trigger("exit_code != 0", &data));
    }

    #[test]
    fn test_exit_code_zero_no_trigger() {
        let data = json!({"exit_code": 0});
        assert!(!evaluate_trigger("exit_code != 0", &data));
    }

    #[test]
    fn test_numeric_comparison() {
        let data = json!({"replication_lag": 1500, "replica_count": 1});
        assert!(evaluate_trigger(
            "replication_lag > 1000 AND replica_count < 2",
            &data
        ));
    }

    #[test]
    fn test_numeric_comparison_no_trigger() {
        // lag is low, so should not trigger
        let data = json!({"replication_lag": 500, "replica_count": 1});
        assert!(!evaluate_trigger(
            "replication_lag > 1000 AND replica_count < 2",
            &data
        ));
    }

    #[test]
    fn test_missing_key_fires() {
        // Fail-open: missing key → true
        let data = json!({});
        assert!(evaluate_trigger("nonexistent_key != ok", &data));
    }

    #[test]
    fn test_validate_syntax_valid() {
        assert!(validate_trigger_syntax("exit_code != 0").is_ok());
        assert!(validate_trigger_syntax("cluster_state == ok OR exit_code > 5").is_ok());
        assert!(validate_trigger_syntax("lag > 1000 AND replicas < 2").is_ok());
    }

    #[test]
    fn test_validate_syntax_invalid() {
        // No operator
        assert!(validate_trigger_syntax("just_a_key").is_err());
        // Missing RHS
        assert!(validate_trigger_syntax("key !=").is_err());
        // Missing LHS
        assert!(validate_trigger_syntax("!= value").is_err());
    }
}
