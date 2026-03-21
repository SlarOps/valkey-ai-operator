//! Filesystem tools: file_read, ls, glob, grep, content_search, file_list
//! Sandboxed to a base directory (typically the skill directory).

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tracing::info;

use crate::agent::tool::{Tool, ToolSafety};
use crate::agent::types::ToolResult;

/// Max file size to read (1MB)
const MAX_FILE_SIZE: u64 = 1_048_576;
/// Max results for glob/grep/ls
const MAX_RESULTS: usize = 500;
/// Max output bytes for grep matches
const MAX_OUTPUT_BYTES: usize = 512_000;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve a path relative to base_dir. Rejects path traversal.
fn resolve_safe_path(base_dir: &Path, requested: &str) -> Result<PathBuf, String> {
    let requested = requested.trim();
    if requested.is_empty() {
        return Ok(base_dir.to_path_buf());
    }

    let candidate = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        base_dir.join(requested)
    };

    // Canonicalize base (must exist)
    let canon_base = base_dir.canonicalize()
        .map_err(|e| format!("base directory error: {}", e))?;

    // For existence check, canonicalize candidate; for non-existent, check parent
    let canon_candidate = if candidate.exists() {
        candidate.canonicalize()
            .map_err(|e| format!("path error: {}", e))?
    } else {
        // Check parent exists and is within base
        let parent = candidate.parent()
            .ok_or_else(|| "invalid path".to_string())?;
        if parent.exists() {
            let canon_parent = parent.canonicalize()
                .map_err(|e| format!("path error: {}", e))?;
            if !canon_parent.starts_with(&canon_base) {
                return Err(format!("path outside allowed directory: {}", requested));
            }
        }
        candidate
    };

    if canon_candidate.exists() && !canon_candidate.starts_with(&canon_base) {
        return Err(format!("path outside allowed directory: {}", requested));
    }

    Ok(canon_candidate)
}

// ---------------------------------------------------------------------------
// FileRead
// ---------------------------------------------------------------------------

pub struct FileRead {
    base_dir: PathBuf,
}

impl FileRead {
    pub fn new(base_dir: &Path) -> Self {
        Self { base_dir: base_dir.to_path_buf() }
    }
}

#[async_trait::async_trait]
impl Tool for FileRead {
    fn name(&self) -> &str { "file_read" }

    fn description(&self) -> &str {
        "Read file contents with optional line offset and limit. Paths are relative to the skill directory."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File path (relative to skill directory)"
                },
                "offset": {
                    "type": "integer",
                    "description": "Starting line number, 1-based (default: 1)"
                },
                "limit": {
                    "type": "integer",
                    "description": "Max lines to return (default: all)"
                }
            },
            "required": ["path"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let path_str = match args["path"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing path".into() },
        };

        let path = match resolve_safe_path(&self.base_dir, path_str) {
            Ok(p) => p,
            Err(e) => return ToolResult { success: false, output: e },
        };

        if !path.exists() {
            return ToolResult { success: false, output: format!("file not found: {}", path_str) };
        }

        if !path.is_file() {
            return ToolResult { success: false, output: format!("not a file: {} (use ls to list directories)", path_str) };
        }

        // Check file size
        if let Ok(meta) = tokio::fs::metadata(&path).await {
            if meta.len() > MAX_FILE_SIZE {
                return ToolResult {
                    success: false,
                    output: format!("file too large: {} bytes (max {}). Use offset/limit to read portions.", meta.len(), MAX_FILE_SIZE),
                };
            }
        }

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => return ToolResult { success: false, output: format!("read error: {}", e) },
        };

        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"].as_u64().map(|l| l as usize);

        let lines: Vec<&str> = content.lines().collect();
        let start = (offset - 1).min(lines.len());
        let end = match limit {
            Some(l) => (start + l).min(lines.len()),
            None => lines.len(),
        };

        let mut output = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            output.push_str(&format!("{:>5}│{}\n", start + i + 1, line));
        }

        if output.is_empty() {
            output = "(empty file)".to_string();
        }

        info!("file_read: {} (lines {}-{} of {})", path_str, start + 1, end, lines.len());
        ToolResult { success: true, output }
    }
}

// ---------------------------------------------------------------------------
// Ls (list directory)
// ---------------------------------------------------------------------------

pub struct Ls {
    base_dir: PathBuf,
}

impl Ls {
    pub fn new(base_dir: &Path) -> Self {
        Self { base_dir: base_dir.to_path_buf() }
    }
}

#[async_trait::async_trait]
impl Tool for Ls {
    fn name(&self) -> &str { "ls" }

    fn description(&self) -> &str {
        "List files and directories. Paths are relative to the skill directory."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path (default: skill root directory)"
                }
            }
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let path_str = args["path"].as_str().unwrap_or(".");

        let path = match resolve_safe_path(&self.base_dir, path_str) {
            Ok(p) => p,
            Err(e) => return ToolResult { success: false, output: e },
        };

        if !path.exists() {
            return ToolResult { success: false, output: format!("directory not found: {}", path_str) };
        }

        if !path.is_dir() {
            return ToolResult { success: false, output: format!("not a directory: {} (use file_read to read files)", path_str) };
        }

        let mut entries = match tokio::fs::read_dir(&path).await {
            Ok(e) => e,
            Err(e) => return ToolResult { success: false, output: format!("read_dir error: {}", e) },
        };

        let mut items: Vec<String> = Vec::new();
        while let Ok(Some(entry)) = entries.next_entry().await {
            if items.len() >= MAX_RESULTS {
                items.push(format!("... (truncated at {} entries)", MAX_RESULTS));
                break;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            let file_type = entry.file_type().await.ok();
            let suffix = if file_type.as_ref().map_or(false, |t| t.is_dir()) { "/" } else { "" };
            let size = if file_type.as_ref().map_or(false, |t| t.is_file()) {
                entry.metadata().await.ok().map(|m| format!("  ({} bytes)", m.len())).unwrap_or_default()
            } else {
                String::new()
            };
            items.push(format!("{}{}{}", name, suffix, size));
        }

        items.sort();

        if items.is_empty() {
            return ToolResult { success: true, output: "(empty directory)".into() };
        }

        info!("ls: {} ({} entries)", path_str, items.len());
        ToolResult { success: true, output: items.join("\n") }
    }
}

// ---------------------------------------------------------------------------
// Glob
// ---------------------------------------------------------------------------

pub struct Glob {
    base_dir: PathBuf,
}

impl Glob {
    pub fn new(base_dir: &Path) -> Self {
        Self { base_dir: base_dir.to_path_buf() }
    }
}

#[async_trait::async_trait]
impl Tool for Glob {
    fn name(&self) -> &str { "glob" }

    fn description(&self) -> &str {
        "Find files matching a glob pattern within the skill directory. Examples: '**/*.md', 'prompts/*.md', '**/*.sh'"
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern (e.g. '**/*.md', 'scripts/**/*.sh')"
                }
            },
            "required": ["pattern"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let pattern = match args["pattern"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing pattern".into() },
        };

        // Reject path traversal
        if pattern.contains("../") || pattern.contains("..\\") {
            return ToolResult { success: false, output: "path traversal not allowed".into() };
        }

        let full_pattern = self.base_dir.join(pattern).to_string_lossy().to_string();

        let matches: Vec<String> = match glob::glob(&full_pattern) {
            Ok(paths) => {
                let base = &self.base_dir;
                paths
                    .filter_map(|p| p.ok())
                    .filter(|p| p.starts_with(base))
                    .take(MAX_RESULTS)
                    .map(|p| {
                        p.strip_prefix(base)
                            .unwrap_or(&p)
                            .to_string_lossy()
                            .to_string()
                    })
                    .collect()
            }
            Err(e) => return ToolResult { success: false, output: format!("invalid pattern: {}", e) },
        };

        if matches.is_empty() {
            return ToolResult { success: true, output: "(no matches)".into() };
        }

        info!("glob: {} ({} matches)", pattern, matches.len());
        ToolResult { success: true, output: matches.join("\n") }
    }
}

// ---------------------------------------------------------------------------
// Grep (content search)
// ---------------------------------------------------------------------------

pub struct Grep {
    base_dir: PathBuf,
}

impl Grep {
    pub fn new(base_dir: &Path) -> Self {
        Self { base_dir: base_dir.to_path_buf() }
    }
}

#[async_trait::async_trait]
impl Tool for Grep {
    fn name(&self) -> &str { "grep" }

    fn description(&self) -> &str {
        "Search file contents by regex pattern within the skill directory. Returns matching lines with file paths and line numbers."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Regex pattern to search for"
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search in (default: skill root)"
                },
                "include": {
                    "type": "string",
                    "description": "File glob filter (e.g. '*.md', '*.rs')"
                },
                "context": {
                    "type": "integer",
                    "description": "Lines of context before and after each match (default: 0)"
                }
            },
            "required": ["pattern"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let pattern = match args["pattern"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing pattern".into() },
        };

        let search_path = args["path"].as_str().unwrap_or(".");
        let path = match resolve_safe_path(&self.base_dir, search_path) {
            Ok(p) => p,
            Err(e) => return ToolResult { success: false, output: e },
        };

        if !path.exists() {
            return ToolResult { success: false, output: format!("path not found: {}", search_path) };
        }

        // Build grep command — prefer rg, fallback to grep
        let context = args["context"].as_u64().unwrap_or(0);
        let include = args["include"].as_str();

        let mut cmd = if which::which("rg").is_ok() {
            let mut c = tokio::process::Command::new("rg");
            c.arg("--no-heading")
             .arg("--line-number")
             .arg("--color=never")
             .arg("--max-count=100");
            if context > 0 {
                c.arg(format!("-C{}", context));
            }
            if let Some(glob) = include {
                c.arg("--glob").arg(glob);
            }
            c.arg(pattern).arg(&path);
            c
        } else {
            let mut c = tokio::process::Command::new("grep");
            c.arg("-rn").arg("-E");
            if context > 0 {
                c.arg(format!("-C{}", context));
            }
            if let Some(glob) = include {
                c.arg("--include").arg(glob);
            }
            c.arg(pattern).arg(&path);
            c
        };

        let output = match cmd.output().await {
            Ok(o) => o,
            Err(e) => return ToolResult { success: false, output: format!("grep error: {}", e) },
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // grep returns exit code 1 for "no matches" — that's not an error
        if !output.status.success() && output.status.code() != Some(1) {
            return ToolResult {
                success: false,
                output: format!("grep failed: {}", stderr),
            };
        }

        if stdout.is_empty() {
            return ToolResult { success: true, output: "(no matches)".into() };
        }

        // Strip base_dir prefix from output for cleaner paths
        let base_str = self.base_dir.to_string_lossy();
        let cleaned = stdout
            .replace(&format!("{}/", base_str), "")
            .replace(&base_str.to_string(), "");

        let result = if cleaned.len() > MAX_OUTPUT_BYTES {
            format!("{}...\n(truncated at {} bytes)", &cleaned[..MAX_OUTPUT_BYTES], MAX_OUTPUT_BYTES)
        } else {
            cleaned.to_string()
        };

        info!("grep: '{}' in {} ({} bytes output)", pattern, search_path, result.len());
        ToolResult { success: true, output: result }
    }
}

// ---------------------------------------------------------------------------
// ContentSearch (alias for grep with different defaults)
// ---------------------------------------------------------------------------

pub struct ContentSearch {
    base_dir: PathBuf,
}

impl ContentSearch {
    pub fn new(base_dir: &Path) -> Self {
        Self { base_dir: base_dir.to_path_buf() }
    }
}

#[async_trait::async_trait]
impl Tool for ContentSearch {
    fn name(&self) -> &str { "content_search" }

    fn description(&self) -> &str {
        "Search for text across all files in the skill directory. Returns file paths containing matches. Use for finding which files mention a topic."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Text or regex to search for"
                },
                "include": {
                    "type": "string",
                    "description": "File glob filter (e.g. '*.md', '*.yaml')"
                }
            },
            "required": ["query"]
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let query = match args["query"].as_str() {
            Some(v) => v,
            None => return ToolResult { success: false, output: "missing query".into() },
        };

        let include = args["include"].as_str();

        // Use rg --files-with-matches or grep -rl
        let mut cmd = if which::which("rg").is_ok() {
            let mut c = tokio::process::Command::new("rg");
            c.arg("--files-with-matches")
             .arg("--color=never");
            if let Some(glob) = include {
                c.arg("--glob").arg(glob);
            }
            c.arg(query).arg(&self.base_dir);
            c
        } else {
            let mut c = tokio::process::Command::new("grep");
            c.arg("-rl").arg("-E");
            if let Some(glob) = include {
                c.arg("--include").arg(glob);
            }
            c.arg(query).arg(&self.base_dir);
            c
        };

        let output = match cmd.output().await {
            Ok(o) => o,
            Err(e) => return ToolResult { success: false, output: format!("search error: {}", e) },
        };

        let stdout = String::from_utf8_lossy(&output.stdout);

        if stdout.is_empty() {
            return ToolResult { success: true, output: "(no matches)".into() };
        }

        // Strip base_dir prefix
        let base_str = self.base_dir.to_string_lossy();
        let cleaned: Vec<&str> = stdout.lines()
            .map(|l| l.strip_prefix(&format!("{}/", base_str)).unwrap_or(l))
            .take(MAX_RESULTS)
            .collect();

        info!("content_search: '{}' ({} files)", query, cleaned.len());
        ToolResult { success: true, output: cleaned.join("\n") }
    }
}

// ---------------------------------------------------------------------------
// FileList (recursive listing with metadata)
// ---------------------------------------------------------------------------

pub struct FileList {
    base_dir: PathBuf,
}

impl FileList {
    pub fn new(base_dir: &Path) -> Self {
        Self { base_dir: base_dir.to_path_buf() }
    }
}

#[async_trait::async_trait]
impl Tool for FileList {
    fn name(&self) -> &str { "file_list" }

    fn description(&self) -> &str {
        "Recursively list all files in a directory with sizes. Useful for understanding skill structure."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path (default: skill root)"
                },
                "max_depth": {
                    "type": "integer",
                    "description": "Max directory depth (default: 5)"
                }
            }
        })
    }

    fn safety(&self) -> ToolSafety { ToolSafety::ReadOnly }

    async fn execute(&self, args: Value) -> ToolResult {
        let path_str = args["path"].as_str().unwrap_or(".");
        let max_depth = args["max_depth"].as_u64().unwrap_or(5) as usize;

        let path = match resolve_safe_path(&self.base_dir, path_str) {
            Ok(p) => p,
            Err(e) => return ToolResult { success: false, output: e },
        };

        if !path.exists() || !path.is_dir() {
            return ToolResult { success: false, output: format!("not a directory: {}", path_str) };
        }

        let mut entries: Vec<String> = Vec::new();
        collect_files(&path, &self.base_dir, 0, max_depth, &mut entries);

        if entries.is_empty() {
            return ToolResult { success: true, output: "(empty)".into() };
        }

        info!("file_list: {} ({} entries)", path_str, entries.len());
        ToolResult { success: true, output: entries.join("\n") }
    }
}

fn collect_files(dir: &Path, base: &Path, depth: usize, max_depth: usize, out: &mut Vec<String>) {
    if depth > max_depth || out.len() >= MAX_RESULTS {
        return;
    }

    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return,
    };
    entries.sort_by_key(|e| e.file_name());

    let indent = "  ".repeat(depth);
    for entry in entries {
        if out.len() >= MAX_RESULTS {
            out.push(format!("{}... (truncated)", indent));
            return;
        }

        let name = entry.file_name().to_string_lossy().to_string();
        // Skip hidden files
        if name.starts_with('.') {
            continue;
        }

        let ft = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };

        if ft.is_dir() {
            out.push(format!("{}{}/", indent, name));
            collect_files(&entry.path(), base, depth + 1, max_depth, out);
        } else if ft.is_file() {
            let size = entry.metadata().ok().map(|m| m.len()).unwrap_or(0);
            out.push(format!("{}{}  ({} bytes)", indent, name, size));
        }
    }
}
