// Filesystem abstraction trait for testability

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// Abstraction over filesystem operations for testability.
pub trait Fs: Send + Sync {
    fn read_to_string(&self, path: &Path) -> Result<String>;
    fn write_atomically(&self, path: &Path, content: &str) -> Result<()>;
    fn rename(&self, from: &Path, to: &Path) -> Result<()>;
    /// Which filesystem `path` sits on. `rename` across two of them fails
    /// with EXDEV.
    fn device_id(&self, path: &Path) -> Result<u64>;
    /// Whether this process may create an entry in `dir`. Mode bits alone do
    /// not say (ownership, ACLs, read-only mounts), so this probes.
    fn probe_writable(&self, dir: &Path) -> Result<()>;
    fn exists(&self, path: &Path) -> bool;
    fn is_dir(&self, path: &Path) -> bool;
    /// The raw target of a symlink, unresolved — a relative target comes
    /// back relative.
    fn read_link(&self, path: &Path) -> Result<PathBuf>;
    /// Whether `path` is a symlink, including a dangling one.
    #[allow(dead_code)]
    fn is_symlink(&self, path: &Path) -> bool;
    /// Repoints the symlink at `path` to `target`, atomically: a new link is
    /// created under a temporary name in the same directory, then renamed
    /// over `path`. A `remove` followed by a `symlink` would leave a hole if
    /// interrupted in between.
    fn replace_symlink(&self, path: &Path, target: &Path) -> Result<()>;
    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>>;
    fn list_dir_recursive(&self, path: &Path) -> Result<Vec<PathBuf>>;
    fn create_dir_all(&self, path: &Path) -> Result<()>;
    #[allow(dead_code)]
    fn remove_dir_all(&self, path: &Path) -> Result<()>;
    #[allow(dead_code)]
    fn copy_file(&self, from: &Path, to: &Path) -> Result<()>;
}

/// Whether `from` could be renamed to `to` right now.
///
/// `rename` fails when the target directory is missing, across filesystems
/// (EXDEV), and into a directory this process may not write. All three only
/// surface once the rename runs, which in a batch is after earlier moves have
/// already landed.
pub fn can_rename(fs: &dyn Fs, from: &Path, to: &Path) -> Result<()> {
    let parent = to
        .parent()
        .with_context(|| format!("no parent directory for {}", to.display()))?;
    if !fs.is_dir(parent) {
        bail!("target directory does not exist: {}", parent.display());
    }

    if fs.device_id(from)? != fs.device_id(parent)? {
        bail!(
            "cross-filesystem move: {} and {} are on different filesystems",
            from.display(),
            parent.display()
        );
    }

    fs.probe_writable(parent)
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

    #[cfg(unix)]
    fn device_id(&self, path: &Path) -> Result<u64> {
        use std::os::unix::fs::MetadataExt as _;
        Ok(std::fs::metadata(path)
            .with_context(|| format!("reading {}", path.display()))?
            .dev())
    }

    /// Without `st_dev` there is nothing to compare, so every path counts as
    /// the same filesystem and the EXDEV branch never fires.
    #[cfg(not(unix))]
    fn device_id(&self, _path: &Path) -> Result<u64> {
        Ok(0)
    }

    fn probe_writable(&self, dir: &Path) -> Result<()> {
        // The file is dropped, and so deleted, at the end of this statement.
        tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("target directory is not writable: {}", dir.display()))?;
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn is_dir(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn read_link(&self, path: &Path) -> Result<PathBuf> {
        std::fs::read_link(path).with_context(|| format!("reading symlink {}", path.display()))
    }

    /// `metadata` follows a symlink and fails when its target is missing;
    /// `symlink_metadata` does not, so a dangling link still reports true.
    fn is_symlink(&self, path: &Path) -> bool {
        std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink())
    }

    #[cfg(unix)]
    fn replace_symlink(&self, path: &Path, target: &Path) -> Result<()> {
        use std::os::unix::fs::symlink;

        let parent = path
            .parent()
            .with_context(|| format!("no parent directory for {}", path.display()))?;
        let file_name = path
            .file_name()
            .with_context(|| format!("no file name for {}", path.display()))?
            .to_string_lossy();
        let tmp_path = parent.join(format!(".ccmv-relink-{file_name}-{}", std::process::id()));

        // A leftover from an interrupted earlier run under the same PID
        // (PIDs recycle) would make `symlink` below fail with EEXIST and
        // wedge every later run that touches this path. `remove_file`
        // removes the symlink entry itself, not whatever it points at.
        if let Err(error) = std::fs::remove_file(&tmp_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(error).with_context(|| {
                format!("clearing stale temporary link at {}", tmp_path.display())
            });
        }

        symlink(target, &tmp_path)
            .with_context(|| format!("creating temporary symlink at {}", tmp_path.display()))?;
        if let Err(error) = std::fs::rename(&tmp_path, path) {
            // The temp link was created but never landed at `path` — remove
            // it so it doesn't sit there as the next run's stale leftover.
            let _ = std::fs::remove_file(&tmp_path);
            return Err(error).with_context(|| format!("replacing symlink at {}", path.display()));
        }
        Ok(())
    }

    /// Without `std::os::unix::fs::symlink` there is no portable way to
    /// create a symlink pointing at an arbitrary target.
    #[cfg(not(unix))]
    fn replace_symlink(&self, _path: &Path, _target: &Path) -> Result<()> {
        bail!("replace_symlink is only supported on unix")
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
        // `entry.file_type()` reports the entry itself, without following a
        // symlink — unlike `path.is_dir()`, which would descend into a
        // directory link's target.
        let file_type = entry
            .file_type()
            .with_context(|| format!("reading file type of {}", path.display()))?;
        if file_type.is_dir() {
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
    /// Mount points and the device they carry, for the EXDEV branch.
    devices: Mutex<HashMap<PathBuf, u64>>,
    /// Directories `probe_writable` refuses.
    readonly: Mutex<HashSet<PathBuf>>,
    /// Symlink path -> raw (possibly relative) target.
    links: Mutex<HashMap<PathBuf, PathBuf>>,
}

#[cfg(test)]
impl MockFs {
    pub fn new() -> Self {
        Self {
            files: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashSet::new()),
            writes: Mutex::new(HashMap::new()),
            devices: Mutex::new(HashMap::new()),
            readonly: Mutex::new(HashSet::new()),
            links: Mutex::new(HashMap::new()),
        }
    }

    /// Puts `mount` and everything under it on its own filesystem.
    pub fn add_mount(&self, mount: &Path, device: u64) {
        self.devices
            .lock()
            .unwrap()
            .insert(mount.to_path_buf(), device);
    }

    pub fn add_readonly_dir(&self, dir: &Path) {
        self.readonly.lock().unwrap().insert(dir.to_path_buf());
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

    /// Registers `path` and its ancestors: a directory whose parent does not
    /// exist is not a state a real filesystem can be in, and `can_rename`
    /// asks about the parent.
    pub fn add_dir(&self, path: &Path) {
        let mut dirs = self.dirs.lock().unwrap();
        for ancestor in path.ancestors() {
            dirs.insert(ancestor.to_path_buf());
        }
    }

    #[allow(dead_code)]
    pub fn get_file(&self, path: &Path) -> Option<String> {
        self.files.lock().unwrap().get(path).cloned()
    }

    /// Seeds a symlink at `path` with the given raw (possibly relative)
    /// target.
    pub fn add_symlink(&self, path: &Path, target: &Path) {
        self.links
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), target.to_path_buf());
    }

    /// Follows `links` from `path` through successive hops, stopping at the
    /// first path that is not itself a link. Bounded so a cycle (link A ->
    /// link B -> link A) terminates instead of looping forever, the way a
    /// real filesystem's ELOOP would stop it.
    fn resolve_symlink_chain(&self, path: &Path) -> PathBuf {
        let links = self.links.lock().unwrap();
        let mut current = path.to_path_buf();
        for _ in 0..40 {
            match links.get(&current) {
                Some(target) => current = target.clone(),
                None => break,
            }
        }
        current
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
        if let Some(parent) = path.parent() {
            self.probe_writable(parent)?;
        }
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

            let mut links = self.links.lock().unwrap();
            let links_to_move: Vec<(PathBuf, PathBuf)> = links
                .iter()
                .filter(|(k, _)| k.starts_with(from))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (old_link, target) in links_to_move {
                links.remove(&old_link);
                let relative = old_link.strip_prefix(from).unwrap();
                links.insert(to.join(relative), target);
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

    /// One filesystem unless a test says otherwise, matched by longest
    /// registered prefix so a whole subtree can be given its own device.
    fn device_id(&self, path: &Path) -> Result<u64> {
        Ok(self
            .devices
            .lock()
            .unwrap()
            .iter()
            .filter(|(mount, _)| path.starts_with(mount))
            .max_by_key(|(mount, _)| mount.components().count())
            .map_or(0, |(_, id)| *id))
    }

    fn probe_writable(&self, dir: &Path) -> Result<()> {
        if self.readonly.lock().unwrap().contains(dir) {
            bail!("target directory is not writable: {}", dir.display());
        }
        Ok(())
    }

    fn exists(&self, path: &Path) -> bool {
        let resolved = self.resolve_symlink_chain(path);
        self.files.lock().unwrap().contains_key(&resolved)
            || self.dirs.lock().unwrap().contains(&resolved)
    }

    fn is_dir(&self, path: &Path) -> bool {
        self.dirs
            .lock()
            .unwrap()
            .contains(&self.resolve_symlink_chain(path))
    }

    fn read_link(&self, path: &Path) -> Result<PathBuf> {
        self.links
            .lock()
            .unwrap()
            .get(path)
            .cloned()
            .with_context(|| format!("not a symlink: {}", path.display()))
    }

    fn is_symlink(&self, path: &Path) -> bool {
        self.links.lock().unwrap().contains_key(path)
    }

    fn replace_symlink(&self, path: &Path, target: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            self.probe_writable(parent)?;
        }
        self.links
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), target.to_path_buf());
        Ok(())
    }

    fn list_dir(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let files = self.files.lock().unwrap();
        let dirs = self.dirs.lock().unwrap();
        let links = self.links.lock().unwrap();
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
        for link_path in links.keys() {
            if link_path.parent() == Some(path) {
                entries.push(link_path.clone());
            }
        }
        Ok(entries)
    }

    fn list_dir_recursive(&self, path: &Path) -> Result<Vec<PathBuf>> {
        let files = self.files.lock().unwrap();
        let links = self.links.lock().unwrap();
        let mut result = Vec::new();
        for file_path in files.keys() {
            if file_path.starts_with(path) && file_path != path {
                result.push(file_path.clone());
            }
        }
        for link_path in links.keys() {
            if link_path.starts_with(path) && link_path != path {
                result.push(link_path.clone());
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

        can_rename(&RealFs, &source, &dir.path().join("moved")).unwrap();
    }

    /// The branch that is portably reachable against the real filesystem. The
    /// other two are asserted against `MockFs`, which can state a second
    /// device and an unwritable directory outright.
    #[test]
    fn real_can_rename_rejects_missing_target_directory() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("proj");
        std::fs::create_dir(&source).unwrap();

        let err = can_rename(&RealFs, &source, &dir.path().join("gone/proj"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn can_rename_rejects_a_different_filesystem() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/x/proj"));
        fs.add_dir(Path::new("/mnt/other"));
        fs.add_mount(Path::new("/mnt"), 42);

        let err = can_rename(&fs, Path::new("/x/proj"), Path::new("/mnt/other/proj"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cross-filesystem"), "{err}");
    }

    #[test]
    fn can_rename_rejects_an_unwritable_target_directory() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/x/proj"));
        fs.add_dir(Path::new("/y"));
        fs.add_readonly_dir(Path::new("/y"));

        let err = can_rename(&fs, Path::new("/x/proj"), Path::new("/y/proj"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not writable"), "{err}");
    }

    #[test]
    fn can_rename_accepts_one_filesystem_and_a_writable_target() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/x/proj"));
        fs.add_dir(Path::new("/y"));

        can_rename(&fs, Path::new("/x/proj"), Path::new("/y/proj")).unwrap();
    }

    /// `std::fs::read_link` returns the target exactly as stored, unresolved.
    /// Pinned so a later "helpful" `canonicalize` can't sneak in — the relink
    /// scan needs the raw relative target, not where it currently resolves.
    #[cfg(unix)]
    #[test]
    fn read_link_returns_raw_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let fs = RealFs;
        let link = dir.path().join("link");
        symlink("relative/target", &link).unwrap();

        assert_eq!(fs.read_link(&link).unwrap(), Path::new("relative/target"));
    }

    /// The true case was pinned from the start; nothing pinned the false one,
    /// so an `is_symlink` that answered yes to everything went unnoticed.
    #[test]
    fn is_symlink_false_for_a_regular_file_and_for_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("regular");
        std::fs::write(&file, "content").unwrap();

        assert!(!RealFs.is_symlink(&file));
        assert!(!RealFs.is_symlink(&dir.path().join("does-not-exist")));
    }

    /// `metadata` follows the link and fails when the target is missing;
    /// `is_symlink` must use `symlink_metadata` instead so a dangling link is
    /// still reported as a symlink.
    #[cfg(unix)]
    #[test]
    fn is_symlink_true_for_dangling_link() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let fs = RealFs;
        let link = dir.path().join("link");
        symlink("/nonexistent/target", &link).unwrap();

        assert!(fs.is_symlink(&link));
    }

    #[cfg(unix)]
    #[test]
    fn replace_symlink_overwrites_existing() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let fs = RealFs;
        let link = dir.path().join("link");
        symlink("old-target", &link).unwrap();

        fs.replace_symlink(&link, Path::new("new-target")).unwrap();

        assert_eq!(fs.read_link(&link).unwrap(), Path::new("new-target"));
    }

    /// The victim is a symlink *pointing at* a directory, not the directory
    /// itself. A wrong implementation (e.g. `remove_dir_all` on the old
    /// target) would delete the target's contents; this pins that it does
    /// not.
    #[cfg(unix)]
    #[test]
    fn replace_symlink_over_directory_link() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let fs = RealFs;
        let victim_target = dir.path().join("victim_dir");
        std::fs::create_dir(&victim_target).unwrap();
        std::fs::write(victim_target.join("keep.txt"), "content").unwrap();

        let link = dir.path().join("link");
        symlink(&victim_target, &link).unwrap();

        fs.replace_symlink(&link, Path::new("new-target")).unwrap();

        assert_eq!(fs.read_link(&link).unwrap(), Path::new("new-target"));
        assert!(victim_target.join("keep.txt").exists());
    }

    /// A stale `.ccmv-relink-*` temp link from an interrupted earlier run
    /// under the same PID (PIDs recycle) must not wedge this one: without
    /// clearing it first, the `symlink` call inside `replace_symlink` fails
    /// with EEXIST every time this path is relinked again.
    #[cfg(unix)]
    #[test]
    fn replace_symlink_clears_stale_temp_link_before_creating() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let fs = RealFs;
        let link = dir.path().join("link");
        symlink("old-target", &link).unwrap();
        let stale_tmp = dir
            .path()
            .join(format!(".ccmv-relink-link-{}", std::process::id()));
        symlink("leftover-target", &stale_tmp).unwrap();

        fs.replace_symlink(&link, Path::new("new-target")).unwrap();

        assert_eq!(fs.read_link(&link).unwrap(), Path::new("new-target"));
    }

    /// When the final `rename` fails, the temp link it created must not be
    /// left behind — otherwise the next call for the same path trips over
    /// the same EEXIST `replace_symlink_clears_stale_temp_link_before_creating`
    /// pins.
    #[cfg(unix)]
    #[test]
    fn replace_symlink_removes_temp_link_after_failed_rename() {
        let dir = tempfile::tempdir().unwrap();
        let fs = RealFs;
        // `rename` cannot replace a directory with a symlink (EISDIR), so
        // the final rename in `replace_symlink` is guaranteed to fail here.
        let path = dir.path().join("link");
        std::fs::create_dir(&path).unwrap();

        assert!(fs.replace_symlink(&path, Path::new("new-target")).is_err());

        let tmp_path = dir
            .path()
            .join(format!(".ccmv-relink-link-{}", std::process::id()));
        assert!(
            std::fs::symlink_metadata(&tmp_path).is_err(),
            "the temporary symlink must not survive a failed rename"
        );
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

    /// `collect_files_recursive` used to branch on `path.is_dir()`, which
    /// follows a symlink into its target and reports the target's contents.
    /// A directory link must be reported as itself, never descended into.
    #[cfg(unix)]
    #[test]
    fn real_list_dir_recursive_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let fs = RealFs;
        let target = dir.path().join("target_dir");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("inside.txt"), "content").unwrap();

        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let link = project.join("link");
        symlink(&target, &link).unwrap();

        let entries = fs.list_dir_recursive(&project).unwrap();

        assert_eq!(entries, vec![link]);
    }

    /// Mirrors `Path::exists`, which follows links: a link whose target was
    /// never registered as a file or directory does not exist.
    #[test]
    fn mock_exists_false_for_dangling_link() {
        let fs = MockFs::new();
        fs.add_symlink(Path::new("/link"), Path::new("/nonexistent/target"));

        assert!(!fs.exists(Path::new("/link")));
    }

    #[test]
    fn mock_is_dir_true_for_link_to_dir() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/real/dir"));
        fs.add_symlink(Path::new("/link"), Path::new("/real/dir"));

        assert!(fs.is_dir(Path::new("/link")));
    }

    /// Renaming a directory carries its link entries along, the way the
    /// existing `rename` already carries `files` and `dirs`.
    #[test]
    fn mock_rename_moves_link_entries() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/old"));
        fs.add_symlink(Path::new("/old/link"), Path::new("some-target"));

        fs.rename(Path::new("/old"), Path::new("/new")).unwrap();

        assert!(fs.is_symlink(Path::new("/new/link")));
        assert!(!fs.is_symlink(Path::new("/old/link")));
        assert_eq!(
            fs.read_link(Path::new("/new/link")).unwrap(),
            Path::new("some-target")
        );
    }

    /// `list_dir` reports a directory link as itself; it must not resolve
    /// through it and report the target's children instead.
    #[test]
    fn mock_list_dir_includes_link_without_descending() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/project"));
        fs.add_dir(Path::new("/other"));
        fs.add_file(Path::new("/other/inside.txt"), "content");
        fs.add_symlink(Path::new("/project/link"), Path::new("/other"));

        let entries = fs.list_dir(Path::new("/project")).unwrap();

        assert_eq!(entries, vec![Path::new("/project/link")]);
    }

    /// Same as `mock_list_dir_includes_link_without_descending`, one level
    /// deeper: the link shows up, its target's contents do not.
    #[test]
    fn mock_list_dir_recursive_includes_link_without_descending() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/project/sub"));
        fs.add_dir(Path::new("/other"));
        fs.add_file(Path::new("/other/inside.txt"), "content");
        fs.add_symlink(Path::new("/project/sub/link"), Path::new("/other"));

        let entries = fs.list_dir_recursive(Path::new("/project")).unwrap();

        assert_eq!(entries, vec![Path::new("/project/sub/link")]);
    }

    /// `MockFs` pinning of the same guarantee as
    /// `real_list_dir_recursive_does_not_follow_symlinks`: a symlink to a
    /// directory outside the scanned tree is reported as itself, never as
    /// its target's contents.
    #[test]
    fn mock_list_dir_recursive_does_not_follow_symlinks() {
        let fs = MockFs::new();
        fs.add_dir(Path::new("/project"));
        fs.add_dir(Path::new("/target_dir"));
        fs.add_file(Path::new("/target_dir/inside.txt"), "content");
        fs.add_symlink(Path::new("/project/link"), Path::new("/target_dir"));

        let entries = fs.list_dir_recursive(Path::new("/project")).unwrap();

        assert_eq!(entries, vec![Path::new("/project/link")]);
    }
}
