//! Optional local code search through the third-party `codedb` CLI. One
//! bounded, read-only process per query: no daemon, no index service and no
//! telemetry (`CODEDB_NO_TELEMETRY=1`). When codedb is not installed the tool
//! says so, and agents keep their harness's own search tools.
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use crate::workspace::run_process;

const MISSING: &str = "codedb is not installed (or not on the app's PATH), so indocode_code_search is unavailable. Install codedb and put it on PATH, or use the harness's own file search tools instead.";
const QUERY_MAX: usize = 4096;
const OUTPUT_LIMIT: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);

/// Locates codedb where an interactive shell would find it, including common
/// per-user installation folders, even when Indocode was opened from Finder
/// with a minimal PATH.
pub(crate) fn executable() -> Option<PathBuf> {
    crate::providers::tool_path("codedb")
}

pub(crate) fn search(root: &Path, query: &str, symbols: bool) -> Result<Value, String> {
    search_with(executable().as_deref(), root, query, symbols)
}

fn search_with(
    binary: Option<&Path>,
    root: &Path,
    query: &str,
    symbols: bool,
) -> Result<Value, String> {
    let query = query.trim();
    if query.is_empty() || query.len() >= QUERY_MAX {
        return Err(format!("query must be 1..{} bytes", QUERY_MAX - 1));
    }
    let binary = binary.ok_or_else(|| MISSING.to_owned())?;
    let command_name = if symbols { "find" } else { "search" };
    let mut command = Command::new(binary);
    command
        .arg(root)
        .arg(command_name)
        .arg("--no-telemetry")
        .arg("--")
        .arg(query)
        .env("CODEDB_NO_TELEMETRY", "1")
        .env("CODEDB_NO_CLI_DAEMON", "1")
        .current_dir(root);
    let output = run_process(command, "codedb", OUTPUT_LIMIT, TIMEOUT)?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr.bytes);
        let reason = reason.trim();
        return Err(if reason.is_empty() {
            format!("codedb {command_name} failed.")
        } else {
            format!("codedb {command_name} failed: {reason}")
        });
    }
    let results = String::from_utf8_lossy(&output.stdout.bytes);
    Ok(json!({
        "query": query,
        "symbols": symbols,
        "truncated": output.stdout.truncated,
        "results": results.trim_end(),
    }))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    fn fake(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A private scratch directory per test; no tempfile dependency.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("indocode-code-search-{name}"));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn search_runs_the_codedb_cli_on_the_checkout_without_telemetry() {
        let temp = Scratch::new("argv");
        let codedb = temp.path().join("codedb");
        let marker = temp.path().join("args");
        fake(
            &codedb,
            &format!(
                "printf '%s|%s|%s|%s|%s|%s|%s' \"$1\" \"$2\" \"$3\" \"$4\" \"$5\" \"$CODEDB_NO_TELEMETRY\" \"$CODEDB_NO_CLI_DAEMON\" > '{}'\nprintf 'src/main.rs:1  fn main()'",
                marker.display()
            ),
        );
        let report =
            search_with(Some(&codedb), temp.path(), "  hello  ", false).unwrap();
        assert_eq!(
            fs::read_to_string(&marker).unwrap(),
            format!("{}|search|--no-telemetry|--|hello|1|1", temp.path().display())
        );
        assert_eq!(report["query"], "hello");
        assert_eq!(report["symbols"], false);
        assert_eq!(report["truncated"], false);
        assert_eq!(report["results"], "src/main.rs:1  fn main()");
    }

    #[test]
    fn symbol_queries_use_find() {
        let temp = Scratch::new("find-flag");
        let codedb = temp.path().join("codedb");
        let marker = temp.path().join("args");
        fake(
            &codedb,
            &format!("printf '%s' \"$2\" > '{}'", marker.display()),
        );
        let report = search_with(Some(&codedb), temp.path(), "main", true).unwrap();
        assert_eq!(fs::read_to_string(&marker).unwrap(), "find");
        assert_eq!(report["symbols"], true);
    }

    #[test]
    fn queries_are_bounded() {
        let temp = Scratch::new("bounded");
        let error = |query: &str| {
            search_with(None, temp.path(), query, false)
                .unwrap_err()
        };
        assert!(error("").contains("query must be"), "empty");
        assert!(error("   ").contains("query must be"), "blank");
        assert!(error(&"x".repeat(QUERY_MAX)).contains("query must be"), "long");
    }

    #[test]
    fn missing_codedb_reports_the_remedy() {
        let temp = Scratch::new("missing");
        let error = search_with(None, temp.path(), "main", false).unwrap_err();
        assert!(error.contains("codedb is not installed"), "{error}");
    }

    #[test]
    fn failures_carry_the_codedb_reason() {
        let temp = Scratch::new("failures");
        let codedb = temp.path().join("codedb");
        fake(&codedb, "echo '  index is locked  ' >&2\nexit 2");
        let error = search_with(Some(&codedb), temp.path(), "main", false).unwrap_err();
        assert_eq!(error, "codedb search failed: index is locked");
        fake(&codedb, "exit 3");
        let error = search_with(Some(&codedb), temp.path(), "main", false).unwrap_err();
        assert_eq!(error, "codedb search failed.");
    }

    #[test]
    fn oversized_results_are_truncated_with_a_flag() {
        let temp = Scratch::new("truncated");
        let codedb = temp.path().join("codedb");
        fake(&codedb, "yes 'src/main.rs:1  fn main()' | head -c 80000");
        let report = search_with(Some(&codedb), temp.path(), "main", false).unwrap();
        assert_eq!(report["truncated"], true);
        assert_eq!(report["results"].as_str().unwrap().len(), OUTPUT_LIMIT);
    }
}
