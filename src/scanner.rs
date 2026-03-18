// State scanner: detects current project migration status

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::encoder;
use crate::fs::Fs;

/// Current state of a Claude Code project and its references.
pub struct ProjectState {
    #[allow(dead_code)]
    pub project_path: PathBuf,
    pub project_dir_exists: bool,
    pub local_claude_dir: Option<PathBuf>,
    pub global_project_dir: Option<PathBuf>,
    pub session_files: Vec<PathBuf>,
    pub sessions_index: Option<PathBuf>,
    pub settings_json: Option<PathBuf>,
    pub mcp_json: Option<PathBuf>,
    pub history_jsonl: Option<PathBuf>,
    pub claude_json: Option<PathBuf>,
    pub paths_consistent: bool,
}

/// Scans a project path and returns its current state.
/// `claude_home` is the path to `~/.claude` (explicit parameter for testability).
pub fn scan(fs: &dyn Fs, project_path: &Path, claude_home: &Path) -> Result<ProjectState> {
    let encoded = encoder::encode(project_path)?;
    let project_dir_exists = fs.is_dir(project_path);

    // Local .claude/ directory
    let local_claude = project_path.join(".claude");
    let local_claude_dir = if fs.is_dir(&local_claude) {
        Some(local_claude.clone())
    } else {
        None
    };

    // settings.json
    let settings_path = local_claude.join("settings.json");
    let settings_json = if fs.exists(&settings_path) {
        Some(settings_path)
    } else {
        None
    };

    // .mcp.json
    let mcp_path = project_path.join(".mcp.json");
    let mcp_json = if fs.exists(&mcp_path) {
        Some(mcp_path)
    } else {
        None
    };

    // Global project directory
    let global_dir = claude_home.join("projects").join(&encoded);
    let global_project_dir;
    let mut session_files = Vec::new();
    let mut sessions_index = None;

    if fs.is_dir(&global_dir) {
        global_project_dir = Some(global_dir.clone());

        // Scan top-level entries in global dir
        let entries = fs.list_dir(&global_dir)?;
        for entry in &entries {
            if let Some(name) = entry.file_name().and_then(|n| n.to_str()) {
                if name == "sessions-index.json" {
                    sessions_index = Some(entry.clone());
                } else if Path::new(name)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
                {
                    session_files.push(entry.clone());
                }
            }

            // Check subdirectories for subagent files
            if fs.is_dir(entry) {
                let subagents_dir = entry.join("subagents");
                if fs.is_dir(&subagents_dir) {
                    let sub_entries = fs.list_dir(&subagents_dir)?;
                    for sub_entry in &sub_entries {
                        if let Some(name) = sub_entry.file_name().and_then(|n| n.to_str())
                            && Path::new(name)
                                .extension()
                                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
                        {
                            session_files.push(sub_entry.clone());
                        }
                    }
                }
            }
        }
    } else {
        global_project_dir = None;
    }

    // history.jsonl
    let history_path = claude_home.join("history.jsonl");
    let history_jsonl = if fs.exists(&history_path) {
        Some(history_path)
    } else {
        None
    };

    // ~/.claude.json (per-project trust state, allowed tools, MCP servers)
    let claude_json = claude_home
        .parent()
        .map(|home| home.join(".claude.json"))
        .filter(|p| fs.exists(p));

    // Determine paths_consistent
    let paths_consistent =
        check_paths_consistent(fs, project_path, &session_files, sessions_index.as_ref());

    Ok(ProjectState {
        project_path: project_path.to_path_buf(),
        project_dir_exists,
        local_claude_dir,
        global_project_dir,
        session_files,
        sessions_index,
        settings_json,
        mcp_json,
        history_jsonl,
        claude_json,
        paths_consistent,
    })
}

/// Checks if paths in session files are consistent with the project path.
/// If there are no files to check, returns `true`.
fn check_paths_consistent(
    fs: &dyn Fs,
    project_path: &Path,
    session_files: &[PathBuf],
    sessions_index: Option<&PathBuf>,
) -> bool {
    let project_str = project_path.to_string_lossy();

    // Try the first session file
    if let Some(first_session) = session_files.first()
        && let Ok(content) = fs.read_to_string(first_session)
    {
        return content.contains(project_str.as_ref());
    }

    // Try sessions-index.json
    if let Some(index_path) = sessions_index
        && let Ok(content) = fs.read_to_string(index_path)
    {
        return content.contains(project_str.as_ref());
    }

    // No files to check — consistent
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::MockFs;
    use std::path::Path;

    fn setup_full_project(fs: &MockFs) {
        let project = Path::new("/home/user/myproject");
        let claude_home = Path::new("/home/user/.claude");
        let encoded = "-home-user-myproject";

        // Project directory
        fs.add_dir(project);
        fs.add_dir(&project.join(".claude"));
        fs.add_file(
            &project.join(".claude/settings.json"),
            r#"{"permissions":{"allow":["Bash(/home/user/myproject/.claude/hooks/hook.py:*)"]}}"#,
        );
        fs.add_file(
            &project.join(".mcp.json"),
            r#"{"servers":{"path":"/home/user/myproject/server"}}"#,
        );

        // Global claude directory
        let global_dir = claude_home.join("projects").join(encoded);
        fs.add_dir(&global_dir);
        fs.add_file(
            &global_dir.join("abc-123.jsonl"),
            r#"{"type":"user","cwd":"/home/user/myproject"}"#,
        );
        fs.add_file(
            &global_dir.join("sessions-index.json"),
            r#"{"entries":[{"projectPath":"/home/user/myproject"}]}"#,
        );

        // Subagent files
        let subagent_dir = global_dir.join("abc-123").join("subagents");
        fs.add_dir(&global_dir.join("abc-123"));
        fs.add_dir(&subagent_dir);
        fs.add_file(
            &subagent_dir.join("agent-a1b2c3.jsonl"),
            r#"{"type":"user","cwd":"/home/user/myproject"}"#,
        );

        // History
        fs.add_file(
            &claude_home.join("history.jsonl"),
            r#"{"project":"/home/user/myproject"}"#,
        );
    }

    #[test]
    fn scan_full_project() {
        let fs = MockFs::new();
        setup_full_project(&fs);

        let state = scan(
            &fs,
            Path::new("/home/user/myproject"),
            Path::new("/home/user/.claude"),
        )
        .unwrap();

        assert!(state.project_dir_exists);
        assert!(state.local_claude_dir.is_some());
        assert!(state.global_project_dir.is_some());
        assert!(state.settings_json.is_some());
        assert!(state.mcp_json.is_some());
        assert!(state.sessions_index.is_some());
        assert!(state.history_jsonl.is_some());
        assert!(state.paths_consistent);
        // Should have session file + subagent file
        assert!(state.session_files.len() >= 2);
    }

    #[test]
    fn scan_minimal_project_no_claude() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/home/user/bare-project"));

        let state = scan(
            &fs,
            Path::new("/home/user/bare-project"),
            Path::new("/home/user/.claude"),
        )
        .unwrap();

        assert!(state.project_dir_exists);
        assert!(state.local_claude_dir.is_none());
        assert!(state.global_project_dir.is_none());
        assert!(state.session_files.is_empty());
        assert!(state.paths_consistent); // No files to check = consistent
    }

    #[test]
    fn scan_moved_project_inconsistent() {
        let fs = MockFs::new();
        let claude_home = Path::new("/home/user/.claude");

        // Project is now at new path
        fs.add_dir(Path::new("/home/user/new-location"));

        // But global refs still point to old path
        let old_encoded = "-home-user-old-location";
        let global_dir = claude_home.join("projects").join(old_encoded);
        fs.add_dir(&global_dir);
        fs.add_file(
            &global_dir.join("session.jsonl"),
            r#"{"cwd":"/home/user/old-location"}"#,
        );

        // Scanning old path (which has global refs but project moved)
        let state = scan(&fs, Path::new("/home/user/old-location"), claude_home).unwrap();

        assert!(!state.project_dir_exists);
        assert!(state.global_project_dir.is_some());
        assert!(!state.session_files.is_empty());
    }

    #[test]
    fn scan_nonexistent_project() {
        let fs = MockFs::new();
        let state = scan(
            &fs,
            Path::new("/nonexistent"),
            Path::new("/home/user/.claude"),
        )
        .unwrap();

        assert!(!state.project_dir_exists);
        assert!(state.local_claude_dir.is_none());
        assert!(state.global_project_dir.is_none());
        assert!(state.paths_consistent);
    }
}
