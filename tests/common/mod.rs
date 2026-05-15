use std::fs;
use std::path::{Path, PathBuf};

/// Creates a realistic Claude Code project structure in the given base directory.
/// Returns (`project_path`, `claude_home_path`).
pub fn setup_claude_project(base: &Path, project_rel_path: &str) -> (PathBuf, PathBuf) {
    let project = base.join(project_rel_path);
    let claude_home = base.join(".claude");

    // Encode the project path (replace all non-alphanumeric chars except - with -)
    let project_abs = project.to_string_lossy().to_string();
    let encoded: String = project_abs
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();

    // Project directory with .claude/
    fs::create_dir_all(project.join(".claude")).unwrap();
    fs::write(
        project.join(".claude/settings.json"),
        format!(
            r#"{{"permissions":{{"allow":["Bash({path}/.claude/hooks/hook.py:*)"]}},"hooks":{{"PreToolUse":[{{"hooks":[{{"type":"command","command":"python {path}/.claude/hooks/redirect.py"}}]}}]}}}}"#,
            path = project.display()
        ),
    )
    .unwrap();
    fs::write(
        project.join(".mcp.json"),
        format!(
            r#"{{"servers":{{"local":{{"command":"node","args":["{path}/server.js"]}}}}}}"#,
            path = project.display()
        ),
    )
    .unwrap();

    // Global claude project directory
    let global_dir = claude_home.join("projects").join(&encoded);
    fs::create_dir_all(&global_dir).unwrap();

    // Session file with cwd references
    fs::write(
        global_dir.join("abc-session.jsonl"),
        format!(
            r#"{{"type":"user","message":{{"role":"user","content":"hello"}},"timestamp":"2026-01-01T00:00:00Z","cwd":"{path}"}}
{{"type":"assistant","message":{{"role":"assistant","content":"hi"}},"timestamp":"2026-01-01T00:00:01Z","cwd":"{path}"}}"#,
            path = project.display()
        ),
    )
    .unwrap();

    // Sessions index
    fs::write(
        global_dir.join("sessions-index.json"),
        format!(
            r#"{{"version":1,"entries":[{{"sessionId":"abc-session","fullPath":"{global}/{encoded}/abc-session.jsonl","projectPath":"{path}"}}]}}"#,
            global = claude_home.join("projects").display(),
            encoded = encoded,
            path = project.display()
        ),
    )
    .unwrap();

    // Subagent directory
    let sub_dir = global_dir.join("abc-session").join("subagents");
    fs::create_dir_all(&sub_dir).unwrap();
    fs::write(
        sub_dir.join("agent-xyz.jsonl"),
        format!(
            r#"{{"type":"user","cwd":"{path}"}}"#,
            path = project.display()
        ),
    )
    .unwrap();

    // ~/.claude.json (per-project trust state)
    let claude_json_path = base.join(".claude.json");
    let project_path_str = project.to_string_lossy().to_string();
    let entry_value = serde_json::json!({
        "hasTrustDialogAccepted": true,
        "allowedTools": [],
        "projectOnboardingSeenCount": 1
    });
    if claude_json_path.exists() {
        let existing = fs::read_to_string(&claude_json_path).unwrap();
        let mut parsed: serde_json::Value = serde_json::from_str(&existing).unwrap();
        parsed[&project_path_str] = entry_value;
        fs::write(&claude_json_path, serde_json::to_string(&parsed).unwrap()).unwrap();
    } else {
        let obj = serde_json::json!({ &project_path_str: entry_value });
        fs::write(&claude_json_path, serde_json::to_string(&obj).unwrap()).unwrap();
    }

    // History.jsonl
    let history_path = claude_home.join("history.jsonl");
    // Append to history if it exists, create otherwise
    let history_entry = format!(
        r#"{{"display":"test","timestamp":1700000000,"project":"{path}"}}"#,
        path = project.display()
    );
    if history_path.exists() {
        let existing = fs::read_to_string(&history_path).unwrap();
        fs::write(&history_path, format!("{existing}\n{history_entry}")).unwrap();
    } else {
        fs::create_dir_all(&claude_home).unwrap();
        fs::write(&history_path, format!("{history_entry}\n")).unwrap();
    }

    (project, claude_home)
}
