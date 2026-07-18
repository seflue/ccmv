// Filesystem abstraction trait for testability

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Abstraction over filesystem operations for testability.
pub trait Fs: Send + Sync {
    fn read_to_string(&self, path: &Path) -> Result<String>;
    fn write_atomically(&self, path: &Path, content: &str) -> Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    /// Whether `from` could be renamed to `to` right now.
    ///
    /// `rename` fails across filesystems (EXDEV) and into a directory this
    /// process may not write. Both only surface once the rename runs, which
    /// in a batch is after earlier moves have already landed.
    fn can_rename(&self, from: &Path, to: &Path) -> Result<()>;
    fn exists(&self, path: &Path) -> bool;
    fn is_dir(&self, path: &Path) -> bool;
    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;
    #[allow(dead_code)]
    fn list_dir_recursive(&self, path: &Path) -> Result<Vec<PathBuf>>;
    #[allow(dead_code)]
    fn create_dir_all(&self, path: &Path) -> Result<()>;
    #[allow(dead_code)]
    fn remove_dir_all(&self, path: &Path) -> Result<()>;
    #[allow(dead_code)]
    fn copy_file(&self, from: &Path, to: &Path) -> Result<()>;
}

/// Real filesystem implementation delegating to `std::fs`.
pub struct RealFs;

impl Fs for RealFs {
    fn read_to_string(&self, path: &Path) -> Result<String> {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
    }

    fn write_atomically(&self, path: &Path, content: &str) -> Result<()> {
        use std::io::Write;

        let parent = path
            .parent()
            .with_context(|| format!("no parent directory for {}", path.display()))?;
        let mut tmp = tempfile::NamedTempFile::new_in(parent).context("creating temporary file")?;
        tmp.write_all(content.as_bytes())
            .context("writing to temporary file")?;
        tmp.persist(path)
            .with_context(|| format!("persisting to {}", path.display()))?;
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        std::fs::rename(from, to)
            .with_context(|| format!("renaming {} -> {}", from.display(), to.display()))
    }

    fn can_rename(&self, from: &Path, to: &Path) -> Result<()> {
        let parent = to
            .parent()
            .with_context(|| format!("no parent directory for {}", to.display()))?;
        if !parent.is_dir() {
            bail!("target directory does not exist: {}", parent.display());
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let from_dev = std::fs::metadata(from)
                .with_context(|| format!("reading {}", from.display()))?
                .dev();
            let to_dev = std::fs::metadata(parent)
                .with_context(|| format!("reading {}", parent.display()))?
                .dev();
            if from_dev != to_dev {
                bail!(
                    "cross-filesystem move: {} and {} are on different filesystems",
                    from.display(),
                    parent.display()
                );
            }
        }

        // Mode bits alone do not say whether this process may create entries
        // here (ownership, ACLs, read-only mounts). Probing does. The file is
        // dropped, and so deleted, at the end of this statement.
        tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("target directory is not writable: {}", parent.display()))?;
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let mut entries = Vec::new();
        for entry in
            std::fs::read_dir(path).with_context(|| format!("listing {}", path.display()))?
        {
            entries.push(entry?.path());
        }
        Ok(entries)
    }

    fn list_dir_recursive(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        collect_files_recursive(path, &mut files)?;
        Ok(files)
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        std::fs::create_dir_all(path)
            .with_context(|| format!("creating directories {}", path.display()))
    }

    fn remove_dir_all(&self, path: &Path) -> Result<()> {
        std::fs::remove_dir_all(path)
            .with_context(|| format!("removing directory {}", path.display()))
    }

    fn copy_file(&self, from: &Path, to: &Path) -> Result<()> {
        std::fs::copy(from, to)
            .map(|_| ())
            .with_context(|| format!("copying {} -> {}", from.display(), to.display()))
    }
}

#[allow(dead_code)]
fn collect_files_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("reading directory {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files_recursive(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// In-memory filesystem for unit tests.
#[cfg(test)]
use std::collections::{HashMap, HashSet};
#[cfg(test)]
use std::sync::Mutex;

#[cfg(test)]
pub struct MockFs {
    files: Mutex<HashMap<PathBuf, String>>,
    dirs: Mutex<HashSet<PathBuf>>,
    /// Counts `write_atomically` calls per path so tests can assert that a
    /// shared file is rewritten once rather than once per project.
    writes: Mutex<HashMap<PathBuf, usize>>,
}

#[cfg(test)]
impl MockFs {
    pub fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashSet::new()),
            writes: Mutex::new(HashMap::new()),
        }
    }

    pub fn write_count(&self, path: &Path) -> usize {
        self.writes.lock().unwrap().get(path).copied().unwrap_or(0)
    }

    pub fn add_file(&self, path: &Path, content: &str) {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), content.to_owned());
    }

    pub fn add_dir(&self, path: &Path) {
        self.dirs.lock().unwrap().insert(path.to_path_buf());
    }

    #[allow(dead_code)]
    pub fn get_file(&self, path: &Path) -> Option<String> {
        self.files.lock().unwrap().get(path).cloned()
    }
}

#[cfg(test)]
impl Fs for MockFs {
    fn read_to_string(&self, path: &Path) -> Result<String> {
        self.files
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .with_context(|| format!("file not found: {}", path.display()))
    }

    fn write_atomically(&self, path: &Path, content: &str) -> Result<()> {
        self.files
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), content.to_owned());
        *self
            .writes
            .lock()
            .unwrap()
            .entry(path.to_path_buf())
            .or_default() += 1;
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> Result<()> {
        let mut files = self.files.lock().unwrap();
        let mut dirs = self.dirs.lock().unwrap();

        // Check if `from` is a directory — if so, move all files and dirs under it.
        if dirs.contains(from) {
            // Collect files to move (can't mutate while iterating)
            let to_move: Vec<(PathBuf, String)> = files
                .iter()
                .filter(|(k, _)| k.starts_with(from))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (old_key, content) in to_move {
                files.remove(&old_key);
                let relative = old_key.strip_prefix(from).unwrap();
                files.insert(to.join(relative), content);
            }

            let dirs_to_move: Vec<PathBuf> = dirs
                .iter()
                .filter(|k| k.starts_with(from))
                .cloned()
                .collect();
            for old_dir in dirs_to_move {
                dirs.remove(&old_dir);
                let relative = old_dir.strip_prefix(from).unwrap();
                dirs.insert(to.join(relative));
            }

            return Ok(());
        }

        // Single file rename
        let content = files
            .remove(from)
            .with_context(|| format!("file not found: {}", from.display()))?;
        files.insert(to.to_path_buf(), content);
        Ok(())
    }

    /// Always fine. Devices and permissions do not exist in memory, and the
    /// missing-parent branch is out of reach too: `dirs` holds only what a
    /// test adds, so an unregistered parent says nothing about the tree.
    /// Unit tests therefore pass this gate vacuously — the check is covered
    /// against `RealFs` and end-to-end instead.
    fn can_rename(&self, _from: &Path, _to: &Path) -> Result<()> {
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        self.files.lock().unwrap().contains_key(path) || self.dirs.lock().unwrap().contains(path)
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.dirs.lock().unwrap().contains(path)
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let files = self.files.lock().unwrap();
        let dirs = self.dirs.lock().unwrap();
        let mut entries = Vec::new();
        for file_path in files.keys() {
            if file_path.parent() == Some(path) {
                entries.push(file_path.clone());
            }
        }
        for dir_path in dirs.iter() {
            if dir_path.parent() == Some(path) {
                entries.push(dir_path.clone());
            }
        }
        Ok(entries)
    }

    fn list_dir_recursive(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let files = self.files.lock().unwrap();
        let mut result = Vec::new();
        for file_path in files.keys() {
            if file_path.starts_with(path) && file_path != path {
                result.push(file_path.clone());
            }
        }
        Ok(result)
    }

    fn create_dir_all(&self, path: &Path) -> Result<()> {
        let mut dirs = self.dirs.lock().unwrap();
        let mut current = path.to_path_buf();
        loop {
            dirs.insert(current.clone());
            match current.parent() {
                Some(parent) if parent != current => current = parent.to_path_buf(),
                _ => break,
            }
        }
        Ok(())
    }

    fn remove_dir_all(&self, path: &Path) -> Result<()> {
        let mut files = self.files.lock().unwrap();
        let mut dirs = self.dirs.lock().unwrap();
        files.retain(|k, _| !k.starts_with(path));
        dirs.retain(|k| !k.starts_with(path));
        Ok(())
    }

    fn copy_file(&self, from: &Path, to: &Path) -> Result<()> {
        let files = self.files.lock().unwrap();
        let content = files
            .get(from)
            .cloned()
            .with_context(|| format!("file not found: {}", from.display()))?;
        drop(files);
        self.files.lock().unwrap().insert(to.to_path_buf(), content);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_counts_writes_per_path() {
        let fs = MockFs::new();
        let path = Path::new("/history.jsonl");
        assert_eq!(fs.write_count(path), 0);

        fs.write_atomically(path, "a").unwrap();
        fs.write_atomically(path, "b").unwrap();

        assert_eq!(fs.write_count(path), 2);
        assert_eq!(fs.write_count(Path::new("/other")), 0);
    }

    /// `add_file` seeds fixtures; it must not count as a write, or every
    /// "written exactly once" assertion would start at 1.
    #[test]
    fn mock_add_file_is_not_a_write() {
        let fs = MockFs::new();
        let path = Path::new("/seeded");
        fs.add_file(path, "content");
        assert_eq!(fs.write_count(path), 0);
    }

    #[test]
    fn mock_read_write_roundtrip() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/tmp"));
        fs.write_atomically(Path::new("/tmp/test.txt"), "hello")
            .unwrap();
        assert_eq!(
            fs.read_to_string(Path::new("/tmp/test.txt")).unwrap(),
            "hello"
        );
    }

    #[test]
    fn mock_exists() {
        let fs = MockFs::new();
        assert!(!fs.exists(Path::new("/nonexistent")));
        fs.add_file(Path::new("/tmp/file"), "content");
        assert!(fs.exists(Path::new("/tmp/file")));
    }

    #[test]
    fn mock_is_dir() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/tmp/mydir"));
        assert!(fs.is_dir(Path::new("/tmp/mydir")));
        assert!(!fs.is_dir(Path::new("/tmp/nonexistent")));
    }

    #[test]
    fn mock_list_dir() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/tmp/project"));
        fs.add_file(Path::new("/tmp/project/a.txt"), "a");
        fs.add_file(Path::new("/tmp/project/b.txt"), "b");
        let mut entries = fs.list_dir(Path::new("/tmp/project")).unwrap();
        entries.sort();
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn mock_rename() {
        let fs = MockFs::new();
        fs.add_file(Path::new("/old"), "content");
        fs.rename(Path::new("/old"), Path::new("/new")).unwrap();
        assert!(!fs.exists(Path::new("/old")));
        assert_eq!(fs.read_to_string(Path::new("/new")).unwrap(), "content");
    }

    #[test]
    fn mock_read_nonexistent_errors() {
        let fs = MockFs::new();
        assert!(fs.read_to_string(Path::new("/nonexistent")).is_err());
    }

    #[test]
    fn real_write_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let fs = RealFs;
        let path = dir.path().join("test.txt");
        fs.write_atomically(&path, "hello world").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
    }

    #[test]
    fn real_can_rename_within_one_directory() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("proj");
        std::fs::create_dir(&source).unwrap();

        RealFs
            .can_rename(&source, &dir.path().join("moved"))
            .unwrap();
    }

    /// The branch that is portably reachable. The other two — a different
    /// filesystem and an unwritable directory — need a second mount and a
    /// non-root user respectively, so neither is asserted here.
    #[test]
    fn real_can_rename_rejects_missing_target_directory() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("proj");
        std::fs::create_dir(&source).unwrap();

        let err = RealFs
            .can_rename(&source, &dir.path().join("gone/proj"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn real_list_dir_recursive() {
        let dir = tempfile::tempdir().unwrap();
        let fs = RealFs;
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(dir.path().join("a.txt"), "a").unwrap();
        std::fs::write(sub.join("b.txt"), "b").unwrap();
        let mut files = fs.list_dir_recursive(dir.path()).unwrap();
        files.sort();
        assert_eq!(files.len(), 2);
    }
}
