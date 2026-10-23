//! The built-in tool catalog for direct-API agents: shell, file read/write/edit
//! and listing. Everything is confined to the task's checkout, bounded in output,
//! and gated by the task's permission mode before it runs.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const OUTPUT_LIMIT: usize = 32 * 1024;
const READ_LIMIT: usize = 512 * 1024;
const LIST_LIMIT: usize = 500;
const TRUNCATED: &str = "\n…[output truncated]";

/// The OpenAI function-calling catalog.
pub fn catalog() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run a shell command in the project folder and see its output. Use for builds, tests, git, package managers. Long-running servers will time out.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "The shell command to run"},
                        "timeout_seconds": {"type": "integer", "description": "Optional timeout, 5-180, default 120"}
                    },
                    "required": ["command"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a text file from the project. Rejects binary files. Optionally read a window of lines.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Path relative to the project folder"},
                        "offset": {"type": "integer", "description": "1-based first line"},
                        "limit": {"type": "integer", "description": "Maximum lines to return (default 400)"}
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create or overwrite a text file in the project with the given content.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Path relative to the project folder"},
                        "content": {"type": "string", "description": "The complete new file content"}
                    },
                    "required": ["path", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Replace an exact string in a text file. The old_string must appear exactly once unless replace_all is set.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Path relative to the project folder"},
                        "old_string": {"type": "string", "description": "The exact text to replace"},
                        "new_string": {"type": "string", "description": "The replacement text"},
                        "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_files",
                "description": "List files in a project folder (respects .gitignore, skips hidden and dependency folders). Use to explore the project.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Folder relative to the project (default the project root)"}
                    }
                }
            }
        }),
    ]
}

pub fn known(name: &str) -> bool {
    matches!(name, "shell" | "read_file" | "write_file" | "edit_file" | "list_files")
}

/// Tools that change the world and therefore ask first (except in Full access).
pub fn is_mutating(name: &str) -> bool {
    matches!(name, "shell" | "write_file" | "edit_file")
}

/// Permission modes: `ask` (Standard) approves every mutating tool, `auto`
/// (Accept edits) only shell, `plan` denies mutation outright, `full` runs all.
pub fn requires_approval(name: &str, mode: &str) -> bool {
    if !is_mutating(name) {
        return false;
    }
    match mode {
        "full" => false,
        "auto" => name == "shell",
        _ => true,
    }
}

/// In plan mode mutation is refused without asking (the task said read-only).
pub fn denied_in_plan(name: &str, mode: &str) -> bool {
    mode == "plan" && is_mutating(name)
}

/// Resolves a model-supplied path inside the checkout; refuses escapes.
/// Tolerates leading separators and a Windows drive prefix; `..` may only
/// cancel folders the model itself supplied, never climb out of the checkout.
fn confine(cwd: &Path, supplied: &str) -> Result<PathBuf, String> {
    let supplied = supplied.trim();
    if supplied.is_empty() || supplied.len() > 4096 {
        return Err("Give a path inside the project folder.".into());
    }
    let cleaned = supplied.replace('\\', "/");
    let cleaned = cleaned.trim_start_matches('/');
    // Drop a leading Windows drive prefix ("C:" or "C:/").
    let cleaned = match cleaned.split_once(':') {
        Some((drive, rest)) if drive.len() == 1 && drive.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) => {
            rest.trim_start_matches('/')
        }
        _ => cleaned,
    };
    let mut parts: Vec<String> = Vec::new();
    for part in cleaned.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Err("Path escapes the project folder.".into());
                }
            }
            _ => parts.push(part.to_owned()),
        }
    }
    if parts.is_empty() {
        return Err("Give a path inside the project folder.".into());
    }
    Ok(cwd.join(parts.iter().collect::<PathBuf>()))
}

fn bounded(text: String) -> String {
    if text.len() <= OUTPUT_LIMIT {
        return text;
    }
    let mut cut = OUTPUT_LIMIT;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{TRUNCATED}", &text[..cut])
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

/// Executes one tool call and returns its (already bounded) result text.
pub async fn execute(name: &str, args: &Value, cwd: &Path) -> Result<String, String> {
    let args = args.as_object().ok_or_else(|| "Tool arguments must be an object.".to_string())?;
    let string_field = |key: &str| -> Result<String, String> {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .ok_or_else(|| format!("Missing the {key} argument."))
    };
    match name {
        "shell" => shell(&string_field("command")?, args.get("timeout_seconds").and_then(|t| t.as_u64()), cwd).await,
        "read_file" => read_file(&string_field("path")?, args, cwd),
        "write_file" => write_file(&string_field("path")?, &string_field("content")?, cwd),
        "edit_file" => edit_file(
            &string_field("path")?,
            &string_field("old_string")?,
            &string_field("new_string")?,
            args.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false),
            cwd,
        ),
        "list_files" => list_files(args.get("path").and_then(|v| v.as_str()).unwrap_or(""), cwd),
        other => Err(format!("Unknown tool {other}.")),
    }
}

async fn shell(command: &str, timeout: Option<u64>, cwd: &Path) -> Result<String, String> {
    if command.len() > 16_384 {
        return Err("Command is too long.".into());
    }
    let seconds = timeout.unwrap_or(120).clamp(5, 180) as u64;
    #[cfg(windows)]
    let mut process = {
        let mut process = tokio::process::Command::new("cmd");
        process.arg("/C").raw_arg(command);
        process
    };
    #[cfg(not(windows))]
    let mut process = {
        let mut process = tokio::process::Command::new("sh");
        process.arg("-c").arg(command);
        process
    };
    let process = process
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(seconds), process.output()).await;
    let output = match outcome {
        Ok(Ok(output)) => output,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err("Could not start a shell in the project folder.".into())
        }
        Ok(Err(error)) => return Err(format!("Shell failed to start: {error}")),
        Err(_) => return Err(format!("The command ran longer than {seconds}s and was stopped.")),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut text = String::new();
    if !stdout.trim().is_empty() {
        text.push_str(&bounded(stdout.into_owned()));
    }
    if !stderr.trim().is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&bounded(stderr.into_owned()));
    }
    match output.status.code() {
        Some(0) => {}
        Some(code) => {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!("Exit code {code}"));
        }
        None => text.push_str("\nTerminated by a signal"),
    }
    Ok(if text.is_empty() { "(no output)".into() } else { text })
}

fn read_file(supplied: &str, args: &serde_json::Map<String, Value>, cwd: &Path) -> Result<String, String> {
    let path = confine(cwd, supplied)?;
    let bytes = std::fs::read(&path).map_err(|e| format!("Could not read {}: {e}", display(&path, cwd)))?;
    if looks_binary(&bytes) {
        return Err("That file looks binary; read it with a shell command instead.".into());
    }
    let text = String::from_utf8_lossy(&bytes);
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
    let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(400).clamp(1, 4000) as usize;
    let selected: Vec<&str> = text
        .lines()
        .skip(offset - 1)
        .take(limit)
        .collect();
    let mut result = String::new();
    for (index, line) in selected.iter().enumerate() {
        result.push_str(&format!("{}|{line}\n", offset + index));
    }
    if result.len() > READ_LIMIT {
        result.truncate(READ_LIMIT);
        result.push_str(TRUNCATED);
    }
    Ok(if result.is_empty() { "(empty or past end of file)".into() } else { result })
}

fn write_file(supplied: &str, content: &str, cwd: &Path) -> Result<String, String> {
    if content.len() > 2 * 1024 * 1024 {
        return Err("Content is too large for one write (2 MiB).".into());
    }
    let path = confine(cwd, supplied)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Could not create the folder: {e}"))?;
    }
    std::fs::write(&path, content).map_err(|e| format!("Could not write {}: {e}", display(&path, cwd)))?;
    Ok(format!("Wrote {} bytes to {}.", content.len(), display(&path, cwd)))
}

fn edit_file(supplied: &str, old: &str, new: &str, replace_all: bool, cwd: &Path) -> Result<String, String> {
    if old.is_empty() {
        return Err("old_string is empty.".into());
    }
    let path = confine(cwd, supplied)?;
    let bytes = std::fs::read(&path).map_err(|e| format!("Could not read {}: {e}", display(&path, cwd)))?;
    if looks_binary(&bytes) {
        return Err("That file looks binary; edit it with a shell command instead.".into());
    }
    let text = String::from_utf8(bytes).map_err(|_| "That file is not valid UTF-8.".to_string())?;
    let count = text.matches(old).count();
    if count == 0 {
        return Err("old_string was not found in the file. Read the file again and copy the exact text.".into());
    }
    if count > 1 && !replace_all {
        return Err(format!("old_string appears {count} times; include more surrounding text or set replace_all."));
    }
    let updated = if replace_all { text.replace(old, new) } else { text.replacen(old, new, 1) };
    std::fs::write(&path, &updated).map_err(|e| format!("Could not write {}: {e}", display(&path, cwd)))?;
    Ok(format!("Edited {} ({} replacement{}).", display(&path, cwd), count.min(if replace_all { count } else { 1 }), if count == 1 { "" } else { "s" }))
}

fn list_files(supplied: &str, cwd: &Path) -> Result<String, String> {
    let root = if supplied.trim().is_empty() { cwd.to_path_buf() } else { confine(cwd, supplied)? };
    if !root.is_dir() {
        return Err("That folder does not exist in the project.".into());
    }
    let mut walker = ignore::WalkBuilder::new(&root);
    walker.hidden(true).git_ignore(true).git_global(false).max_depth(Some(6));
    let mut entries: Vec<String> = Vec::new();
    for entry in walker.build().flatten() {
        if entries.len() >= LIST_LIMIT {
            entries.push(format!("…[stopped at {LIST_LIMIT} entries]"));
            break;
        }
        let Ok(relative) = entry.path().strip_prefix(&root) else { continue };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        entries.push(if is_dir {
            format!("{}/", relative.display())
        } else {
            relative.display().to_string()
        });
    }
    Ok(if entries.is_empty() { "(no files)".into() } else { entries.join("\n") })
}

fn display(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd).unwrap_or(path).display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_project() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("indocode-tools-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn shell_runs_in_the_project_and_reports_exit_codes() {
        let cwd = temp_project();
        let out = execute("shell", &json!({"command": "echo hello"}), &cwd).await.unwrap();
        assert!(out.contains("hello"), "{out}");
        let out = execute("shell", &json!({"command": "exit 3"}), &cwd).await.unwrap();
        assert!(out.contains("Exit code 3"), "{out}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn paths_cannot_escape_the_checkout() {
        let cwd = temp_project();
        assert!(confine(&cwd, "src/main.ts").is_ok());
        assert!(confine(&cwd, "../outside.txt").is_err());
        assert!(confine(&cwd, "a/../../outside").is_err());
        assert!(confine(&cwd, "").is_err());
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn write_read_edit_roundtrip() {
        let cwd = temp_project();
        execute("write_file", &json!({"path": "src/app.ts", "content": "const a = 1;\nconst b = 2;\n"}), &cwd).await.unwrap();
        let read = execute("read_file", &json!({"path": "src/app.ts"}), &cwd).await.unwrap();
        assert!(read.contains("1|const a = 1;"), "{read}");
        let edited = execute("edit_file", &json!({"path": "src/app.ts", "old_string": "const b = 2;", "new_string": "const b = 3;"}), &cwd).await.unwrap();
        assert!(edited.contains("Edited"), "{edited}");
        let missed = execute("edit_file", &json!({"path": "src/app.ts", "old_string": "nope", "new_string": "x"}), &cwd).await;
        assert!(missed.unwrap_err().contains("was not found"));
        let listed = execute("list_files", &json!({}), &cwd).await.unwrap();
        assert!(listed.contains("src/"), "{listed}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[tokio::test]
    async fn binary_files_are_refused() {
        let cwd = temp_project();
        std::fs::write(cwd.join("blob.bin"), [0u8, 1, 0, 2]).unwrap();
        let error = execute("read_file", &json!({"path": "blob.bin"}), &cwd).await.unwrap_err();
        assert!(error.contains("binary"), "{error}");
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn approval_policy_matches_the_modes() {
        assert!(requires_approval("shell", "ask"));
        assert!(requires_approval("write_file", "ask"));
        assert!(!requires_approval("read_file", "ask"));
        assert!(requires_approval("shell", "auto"));
        assert!(!requires_approval("edit_file", "auto"));
        assert!(!requires_approval("shell", "full"));
        assert!(denied_in_plan("shell", "plan"));
        assert!(!denied_in_plan("read_file", "plan"));
        assert!(known("shell") && !known("web_search"));
    }
}
