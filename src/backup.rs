// Backup manager: creates and restores tar.gz backups of Claude references

use std::io::Read as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use tar::{Archive, Builder, Header};

use crate::encoder;

#[derive(Debug, Serialize, Deserialize)]
pub struct BackupManifest {
    pub source_path: PathBuf,
    pub claude_home: PathBuf,
    pub timestamp: String,
    pub version: String,
}

/// Creates a tar.gz backup of all Claude Code references for a project.
/// Returns the path to the created backup file.
/// Location: `{claude_home}/backups/ccmv/{encoded}-{YYYYMMDD-HHMMSS}.tar.gz`
pub fn create_backup(
    project_path: &Path,
    claude_home: &Path,
    global_project_dir: &Path,
    local_claude_dir: Option<&Path>,
    mcp_json: Option<&Path>,
    claude_json: Option<&Path>,
) -> Result<PathBuf> {
    let backup_dir = claude_home.join("backups/ccmv");
    std::fs::create_dir_all(&backup_dir)
        .with_context(|| format!("creating backup directory {}", backup_dir.display()))?;

    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let encoded = encoder::encode(project_path)?;
    let backup_path = backup_dir.join(format!("{encoded}-{timestamp}.tar.gz"));

    let file = std::fs::File::create(&backup_path)
        .with_context(|| format!("creating backup file {}", backup_path.display()))?;
    let enc = GzEncoder::new(file, Compression::default());
    let mut builder = Builder::new(enc);

    // Manifest
    let manifest = BackupManifest {
        source_path: project_path.to_path_buf(),
        claude_home: claude_home.to_path_buf(),
        timestamp: timestamp.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    let manifest_json =
        serde_json::to_string_pretty(&manifest).context("serializing backup manifest")?;
    append_bytes(&mut builder, "manifest.json", manifest_json.as_bytes())?;

    // Global project directory contents
    if global_project_dir.is_dir() {
        append_dir_recursive(&mut builder, global_project_dir, Path::new("global"))?;
    }

    // Local .claude/ directory
    if let Some(local_dir) = local_claude_dir
        && local_dir.is_dir()
    {
        append_dir_recursive(&mut builder, local_dir, Path::new("local/.claude"))?;
    }

    // .mcp.json
    if let Some(mcp) = mcp_json
        && mcp.is_file()
    {
        let content = std::fs::read(mcp).with_context(|| format!("reading {}", mcp.display()))?;
        append_bytes(&mut builder, "local/.mcp.json", &content)?;
    }

    // ~/.claude.json (per-project trust state)
    if let Some(cj) = claude_json
        && cj.is_file()
    {
        let content = std::fs::read(cj).with_context(|| format!("reading {}", cj.display()))?;
        append_bytes(&mut builder, "claude.json", &content)?;
    }

    let enc = builder.into_inner().context("finishing tar archive")?;
    enc.finish().context("finishing gzip compression")?;

    Ok(backup_path)
}

/// Restores a backup from a tar.gz archive.
/// Reads the manifest to determine where files should be restored.
pub fn restore_backup(backup_path: &Path) -> Result<BackupManifest> {
    let file = std::fs::File::open(backup_path)
        .with_context(|| format!("opening backup {}", backup_path.display()))?;
    let dec = GzDecoder::new(file);
    let mut archive = Archive::new(dec);

    let mut manifest: Option<BackupManifest> = None;

    for entry_result in archive.entries().context("reading tar entries")? {
        let mut entry = entry_result.context("reading tar entry")?;
        let path = entry.path().context("reading entry path")?.into_owned();
        let path_str = path.to_string_lossy().to_string();

        if path_str == "manifest.json" {
            let mut content = String::new();
            entry
                .read_to_string(&mut content)
                .context("reading manifest.json")?;
            manifest = Some(serde_json::from_str(&content).context("parsing manifest.json")?);
            continue;
        }

        let m = manifest
            .as_ref()
            .context("manifest.json must be the first entry in the archive")?;

        if let Some(rest) = path_str.strip_prefix("global/") {
            let encoded = encoder::encode(&m.source_path)?;
            let target = m.claude_home.join("projects").join(&encoded).join(rest);
            extract_entry_to(&mut entry, &target)?;
        } else if let Some(rest) = path_str.strip_prefix("local/") {
            let target = m.source_path.join(rest);
            extract_entry_to(&mut entry, &target)?;
        }
    }

    manifest.context("backup archive did not contain manifest.json")
}

/// Appends raw bytes as a file entry in the tar archive.
fn append_bytes(
    builder: &mut Builder<GzEncoder<std::fs::File>>,
    archive_path: &str,
    data: &[u8],
) -> Result<()> {
    let mut header = Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, archive_path, data)
        .with_context(|| format!("appending {archive_path} to archive"))
}

/// Recursively adds all files under `source_dir` to the archive under `archive_prefix`.
fn append_dir_recursive(
    builder: &mut Builder<GzEncoder<std::fs::File>>,
    source_dir: &Path,
    archive_prefix: &Path,
) -> Result<()> {
    for entry in walkdir(source_dir)? {
        let relative = entry.strip_prefix(source_dir).with_context(|| {
            format!(
                "stripping prefix {} from {}",
                source_dir.display(),
                entry.display()
            )
        })?;
        let archive_path = archive_prefix.join(relative);
        // Skip broken symlinks: `read` follows symlinks, so a dangling link
        // would otherwise abort the whole backup. Use `symlink_metadata` to
        // detect symlinks without following, then check the resolved target.
        let is_symlink =
            std::fs::symlink_metadata(&entry).is_ok_and(|m| m.file_type().is_symlink());
        if is_symlink && !entry.exists() {
            eprintln!("warning: skipping broken symlink {}", entry.display());
            continue;
        }
        let content = match std::fs::read(&entry) {
            Ok(c) => c,
            Err(e) if is_symlink => {
                eprintln!(
                    "warning: skipping unreadable symlink {}: {e}",
                    entry.display()
                );
                continue;
            }
            Err(e) => {
                return Err(anyhow::Error::from(e).context(format!("reading {}", entry.display())));
            }
        };
        append_bytes(builder, &archive_path.to_string_lossy(), &content)?;
    }
    Ok(())
}

/// Walks a directory recursively, returning all file paths (not directories).
fn walkdir(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect_files(dir, &mut files)?;
    Ok(files)
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// Extracts a tar entry to the given target path, creating parent directories as needed.
fn extract_entry_to(
    entry: &mut tar::Entry<'_, GzDecoder<std::fs::File>>,
    target: &Path,
) -> Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    let mut content = Vec::new();
    entry
        .read_to_end(&mut content)
        .with_context(|| format!("reading entry for {}", target.display()))?;
    std::fs::write(target, &content).with_context(|| format!("writing {}", target.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup_test_project(base: &Path) -> (PathBuf, PathBuf) {
        let project = base.join("myproject");
        let claude_home = base.join(".claude");

        // Project with .claude/ dir
        fs::create_dir_all(project.join(".claude")).unwrap();
        fs::write(project.join(".claude/settings.json"), r#"{"test": true}"#).unwrap();
        fs::write(project.join(".mcp.json"), r#"{"servers": {}}"#).unwrap();

        // Global claude project dir
        let encoded = crate::encoder::encode(&project).unwrap();
        let global_dir = claude_home.join("projects").join(&encoded);
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(global_dir.join("session-abc.jsonl"), r#"{"cwd":"/test"}"#).unwrap();
        fs::write(global_dir.join("sessions-index.json"), r#"{"version": 1}"#).unwrap();

        // Subagent dir
        let sub_dir = global_dir.join("session-abc").join("subagents");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(sub_dir.join("agent-xyz.jsonl"), r#"{"type":"agent"}"#).unwrap();

        (project, claude_home)
    }

    #[test]
    fn create_backup_produces_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let (project, claude_home) = setup_test_project(tmp.path());
        let encoded = crate::encoder::encode(&project).unwrap();
        let global_dir = claude_home.join("projects").join(&encoded);

        let backup_path = create_backup(
            &project,
            &claude_home,
            &global_dir,
            Some(&project.join(".claude")),
            Some(&project.join(".mcp.json")),
            None,
        )
        .unwrap();

        assert!(backup_path.exists());
        assert!(backup_path.to_string_lossy().ends_with(".tar.gz"));
        // Verify it's in the right location
        assert!(backup_path.starts_with(claude_home.join("backups/ccmv")));
    }

    #[test]
    fn backup_restore_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let (project, claude_home) = setup_test_project(tmp.path());
        let encoded = crate::encoder::encode(&project).unwrap();
        let global_dir = claude_home.join("projects").join(&encoded);

        // Create backup
        let backup_path = create_backup(
            &project,
            &claude_home,
            &global_dir,
            Some(&project.join(".claude")),
            Some(&project.join(".mcp.json")),
            None,
        )
        .unwrap();

        // Delete originals
        fs::remove_dir_all(&global_dir).unwrap();
        fs::remove_dir_all(project.join(".claude")).unwrap();
        fs::remove_file(project.join(".mcp.json")).unwrap();

        // Restore
        let manifest = restore_backup(&backup_path).unwrap();
        assert_eq!(manifest.source_path, project);

        // Verify restored files
        assert!(global_dir.join("session-abc.jsonl").exists());
        assert!(global_dir.join("sessions-index.json").exists());
        assert!(
            global_dir
                .join("session-abc/subagents/agent-xyz.jsonl")
                .exists()
        );
        assert!(project.join(".claude/settings.json").exists());
        assert!(project.join(".mcp.json").exists());
    }

    /// Bug 3: `append_dir_recursive` used `std::fs::read` which follows
    /// symlinks. A broken symlink under the local `.claude/` tree (common
    /// e.g. when skills were removed but symlinks remain) would abort the
    /// whole backup. Broken symlinks must be skipped with a warning.
    #[cfg(unix)]
    #[test]
    fn create_backup_skips_broken_symlinks_in_local_claude_dir() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let (project, claude_home) = setup_test_project(tmp.path());
        let encoded = crate::encoder::encode(&project).unwrap();
        let global_dir = claude_home.join("projects").join(&encoded);

        // Add a broken symlink inside .claude/
        let skills = project.join(".claude/skills");
        fs::create_dir_all(&skills).unwrap();
        symlink("/nonexistent/target", skills.join("broken-link")).unwrap();

        // Must succeed — broken symlink is skipped, not propagated.
        let backup_path = create_backup(
            &project,
            &claude_home,
            &global_dir,
            Some(&project.join(".claude")),
            Some(&project.join(".mcp.json")),
            None,
        )
        .unwrap();
        assert!(backup_path.exists());
    }

    #[test]
    fn create_backup_without_optional_files() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_home = tmp.path().join(".claude");
        let project = tmp.path().join("bare-project");
        fs::create_dir_all(&project).unwrap();

        let encoded = crate::encoder::encode(&project).unwrap();
        let global_dir = claude_home.join("projects").join(&encoded);
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(global_dir.join("session.jsonl"), "{}").unwrap();

        let backup_path = create_backup(
            &project,
            &claude_home,
            &global_dir,
            None, // no local .claude/
            None, // no .mcp.json
            None, // no .claude.json
        )
        .unwrap();

        assert!(backup_path.exists());
    }
}
