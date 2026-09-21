//! The on-disk backend: artifacts under `.cargo-hole/`, plus an append-only
//! ledger.
//!
//! Two properties matter here and everything else follows from them.
//!
//! **The ledger is append-only.** Nothing is ever rewritten in place, so there
//! is no read-modify-write window in which two processes can lose each other's
//! entries, and a crash can only ever truncate the *tail*. A newer entry for an
//! id supersedes the older one on read, so "update" is spelled "append".
//!
//! **Artifacts land atomically.** A half-written artifact would be read back by
//! a later `--check` as though it were real output, so the write goes to a
//! temporary file in the destination directory and is then renamed over the
//! target, which is atomic within a filesystem.
//!
//! The order between the two is deliberate: an artifact is written **first**, its
//! ledger entry second. Crash in between and the ledger is simply missing an
//! entry, which costs one regeneration on the next run. The reverse order would
//! leave the ledger claiming that code exists which does not, and no later run
//! could tell the difference.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Context, Result, bail};

use super::{Entry, LEDGER_FILE, SCHEMA, STORE_DIR};

/// Counter for unique temporary filenames within this process.
static NEXT_TEMP: AtomicU32 = AtomicU32::new(0);

/// Artifacts and the ledger, read into memory and appended as fills land.
#[derive(Debug)]
pub struct DiskCache {
    /// The crate root, as given to [`DiskCache::open`].
    root: PathBuf,
    /// `<root>/.cargo-hole`, where everything this backend writes lives.
    dir: PathBuf,
    /// Every entry read or written so far, in file order, grouped by hole id.
    ///
    /// A `Vec` rather than a single entry: the newest one answers a lookup, but
    /// keeping the history is what lets a reviewer see that an answer was
    /// replaced, and by which agent.
    by_id: HashMap<String, Vec<Entry>>,
}

impl DiskCache {
    /// Open the store rooted at `root`, reading whatever ledger is already
    /// there.
    ///
    /// Creates nothing. `list` and `--check` run against crates that have never
    /// been filled, and a read-only query should not leave a directory behind;
    /// the store appears when something is first written.
    pub fn open(root: &Path) -> Result<DiskCache> {
        let dir = root.join(STORE_DIR);
        let by_id = load_ledger(&dir.join(LEDGER_FILE))?;
        Ok(DiskCache {
            root: root.to_path_buf(),
            dir,
            by_id,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where generated code for source file `file` belongs:
    /// `<root>/.cargo-hole/<file relative to root>`.
    ///
    /// Refuses a path outside the root, or one containing `..`, `.` or a root
    /// component. `util::rust_files` only ever yields paths under the root, so
    /// reaching this is a bug -- but the cost of being wrong is generated code
    /// landing somewhere unexpected, and the check is cheap.
    pub fn artifact_path(&self, file: &Path) -> Result<PathBuf> {
        let rel = file.strip_prefix(&self.root).with_context(|| {
            format!(
                "{} is not inside the crate root {}",
                file.display(),
                self.root.display()
            )
        })?;
        if rel.as_os_str().is_empty() {
            bail!("{} is the crate root, not a source file", file.display());
        }
        for component in rel.components() {
            // `Normal` is the only component allowed, which rejects `..`, `.`
            // and a leading `/` outright: an artifact can never be written
            // outside the store.
            if !matches!(component, Component::Normal(_)) {
                bail!(
                    "{} has a path component that is not a plain name, so it cannot be \
                     mapped into {}",
                    file.display(),
                    self.dir.display()
                );
            }
        }
        Ok(self.dir.join(rel))
    }

    /// Write `src` as the artifact for `file`, replacing any earlier one.
    ///
    /// Atomic: a reader either sees the previous artifact or the new one, never
    /// a mixture, so an interrupted run cannot corrupt output that a later
    /// `--check` would trust.
    pub fn write_artifact(&self, file: &Path, src: &str) -> Result<PathBuf> {
        let dest = self.artifact_path(file)?;
        let dir = dest
            .parent()
            .with_context(|| format!("{} has no parent directory", dest.display()))?;
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;

        // The temporary file must share a directory with the target: `rename` is
        // only atomic within one filesystem, and /tmp is often a different one.
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "artifact".to_string());
        let tmp = dir.join(format!(
            ".{name}.tmp-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed),
        ));

        if let Err(e) = std::fs::write(&tmp, src) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("cannot write {}", tmp.display()));
        }
        if let Err(e) = std::fs::rename(&tmp, &dest) {
            // Leaving the temporary file behind would be litter, and a later
            // `--check` would read it as an artifact.
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("cannot replace {}", dest.display()));
        }
        Ok(dest)
    }

    /// The most recent entry for `id`, verified or not. For reporting.
    pub fn lookup(&self, id: &str) -> Option<&Entry> {
        self.by_id.get(id).and_then(|entries| entries.last())
    }

    /// The most recent entry for `id` that a compile gate accepted.
    ///
    /// Scanning backwards rather than looking only at the newest entry is
    /// deliberate. A regeneration that fails the gate appends nothing, so the
    /// newest entry can be an unverified candidate while the artifact on disk
    /// still holds an older verified answer. Returning that older entry is what
    /// matches what the file actually contains.
    pub fn lookup_verified(&self, id: &str) -> Option<&Entry> {
        self.by_id
            .get(id)?
            .iter()
            .rev()
            .find(|entry| entry.verified)
    }

    /// Append `entry` to the ledger and index it.
    ///
    /// Call this only once the artifact is on disk; see the module docs for why
    /// the order is not interchangeable.
    ///
    /// The write is not fsynced. A lost tail entry costs one regeneration, which
    /// is always correct, while an fsync per hole would dominate the runtime of
    /// a run that is otherwise network-bound.
    pub fn record(&mut self, entry: Entry) -> Result<()> {
        self.append(&entry)?;
        self.by_id.entry(entry.id.clone()).or_default().push(entry);
        Ok(())
    }

    /// Mark every unverified entry from `session` as verified, returning how
    /// many were newly marked.
    ///
    /// Implemented as an append of updated copies, so the ledger stays
    /// append-only and the existing bytes are never rewritten. Idempotent:
    /// entries already marked are skipped, so a second call appends nothing.
    pub fn mark_session_verified(&mut self, session: u64) -> Result<usize> {
        let mut confirmed: Vec<Entry> = Vec::new();
        for entries in self.by_id.values() {
            for entry in entries {
                if entry.session == session && !entry.verified {
                    let mut updated = entry.clone();
                    updated.verified = true;
                    confirmed.push(updated);
                }
            }
        }
        if confirmed.is_empty() {
            return Ok(0);
        }
        for entry in &confirmed {
            self.append(entry)?;
        }
        let count = confirmed.len();
        for entry in confirmed {
            self.by_id.entry(entry.id.clone()).or_default().push(entry);
        }
        Ok(count)
    }

    /// How many distinct holes the ledger knows about, verified or not.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the ledger knows about no holes at all.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// How many holes have an answer a compile gate accepted.
    pub fn verified_count(&self) -> usize {
        self.by_id
            .values()
            .filter(|entries| entries.iter().any(|entry| entry.verified))
            .count()
    }

    /// Append one entry as a line of JSON.
    fn append(&self, entry: &Entry) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("cannot create {}", self.dir.display()))?;
        let ledger = self.dir.join(LEDGER_FILE);
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ledger)
            .with_context(|| format!("cannot open {}", ledger.display()))?;
        // One `write_all` of one line ending in `\n`. Under `O_APPEND` a single
        // small write is not interleaved with a concurrent appender's, which is
        // what lets two runs share a ledger without a lock.
        let line = serde_json::to_string(entry).context("cannot serialize a ledger entry")?;
        writeln!(file, "{line}").with_context(|| format!("cannot append to {}", ledger.display()))
    }
}

/// Read the ledger at `path` into entries grouped by hole id, in file order.
///
/// A missing ledger is an empty one. An unreadable *last* line is tolerated
/// silently: a tear in the tail is the expected shape of a crash during an
/// append, so warning about it would cry wolf on the one corruption the format
/// is designed to survive. Damage anywhere else means something unexpected
/// happened and is worth a warning, but it still must not fail the run -- the
/// entries around it are intact, and regenerating the rest is always correct.
fn load_ledger(path: &Path) -> Result<HashMap<String, Vec<Entry>>> {
    let mut by_id: HashMap<String, Vec<Entry>> = HashMap::new();

    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(by_id),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };

    let lines: Vec<&str> = text.lines().collect();
    let last_content = lines.iter().rposition(|line| !line.trim().is_empty());

    for (n, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(line) {
            // The only entries that count. A different schema version means the
            // fields may no longer mean what this binary thinks they mean, so
            // the entry is ignored rather than misread: regenerating it costs
            // one model call, misreading it could splice the wrong code.
            Ok(entry) if entry.v == SCHEMA => {
                by_id.entry(entry.id.clone()).or_default().push(entry);
            }
            Ok(_) => {}
            Err(_) if Some(n) == last_content => {}
            Err(e) => eprintln!(
                "warning: {} line {} is unreadable and was skipped: {e}",
                path.display(),
                n + 1
            ),
        }
    }

    Ok(by_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    static NEXT_ROOT: AtomicU32 = AtomicU32::new(0);

    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "cargo-hole-disk-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("create the temp root");
        root
    }

    #[test]
    fn an_artifact_mirrors_the_source_tree() {
        let root = temp_root();
        let cache = DiskCache::open(&root).expect("open");
        let path = cache
            .artifact_path(&root.join("src/thing/mod.rs"))
            .expect("mapped");
        assert_eq!(path, root.join(STORE_DIR).join("src/thing/mod.rs"));
    }

    #[test]
    fn an_artifact_can_never_escape_the_store() {
        let root = temp_root();
        let cache = DiskCache::open(&root).expect("open");
        assert!(
            cache
                .artifact_path(Path::new("/somewhere/else.rs"))
                .is_err(),
            "a file outside the crate root has nowhere to go"
        );
        assert!(
            cache.artifact_path(&root).is_err(),
            "the crate root is not a source file"
        );
    }

    #[test]
    fn writing_an_artifact_replaces_the_previous_one() {
        let root = temp_root();
        let cache = DiskCache::open(&root).expect("open");
        let file = root.join("src/lib.rs");

        let first = cache.write_artifact(&file, "one").expect("write");
        assert_eq!(std::fs::read_to_string(&first).expect("read"), "one");
        let second = cache.write_artifact(&file, "two").expect("write");
        assert_eq!(second, first, "the artifact path is stable");
        assert_eq!(std::fs::read_to_string(&second).expect("read"), "two");
    }

    #[test]
    fn writing_an_artifact_creates_its_directories() {
        let root = temp_root();
        let cache = DiskCache::open(&root).expect("open");
        let file = root.join("src/deeply/nested/mod.rs");
        let written = cache.write_artifact(&file, "code").expect("write");
        assert!(written.exists());
    }

    #[test]
    fn a_write_leaves_no_temporary_files_behind() {
        let root = temp_root();
        let cache = DiskCache::open(&root).expect("open");
        let file = root.join("src/lib.rs");
        cache.write_artifact(&file, "one").expect("write");
        cache.write_artifact(&file, "two").expect("write");

        let dir = root.join(STORE_DIR).join("src");
        let names: Vec<String> = std::fs::read_dir(&dir)
            .expect("list the store")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["lib.rs".to_string()],
            "a finished write must not litter, or a later `--check` would \
             mistake a temporary file for an artifact"
        );
    }

    #[test]
    fn a_missing_ledger_reads_as_empty() {
        let root = temp_root();
        let cache = DiskCache::open(&root).expect("open");
        assert!(cache.is_empty());
        assert_eq!(cache.verified_count(), 0);
    }

    #[test]
    fn entries_are_indexed_in_file_order() {
        let root = temp_root();
        let mut cache = DiskCache::open(&root).expect("open");
        cache
            .record(Entry::new("a", "first", "agent"))
            .expect("record");
        cache
            .record(Entry::new("a", "second", "agent"))
            .expect("record");
        assert_eq!(cache.lookup("a").expect("found").code, "second");
    }

    #[test]
    fn the_ledger_is_one_json_object_per_line() {
        let root = temp_root();
        let mut cache = DiskCache::open(&root).expect("open");
        cache.record(Entry::new("a", "1", "agent")).expect("record");
        cache.record(Entry::new("b", "2", "agent")).expect("record");

        let text = std::fs::read_to_string(root.join(STORE_DIR).join(LEDGER_FILE)).expect("read");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert_eq!(value["v"], SCHEMA);
        }
    }

    #[test]
    fn marking_a_session_appends_rather_than_rewriting() {
        // The ledger is append-only, so confirming a run must not disturb the
        // bytes already written -- another process may be reading them.
        let root = temp_root();
        let mut cache = DiskCache::open(&root).expect("open");
        let session = super::super::new_session();
        let mut entry = Entry::new("a", "1", "agent");
        entry.session = session;
        cache.record(entry).expect("record");

        let ledger = root.join(STORE_DIR).join(LEDGER_FILE);
        let before = std::fs::read_to_string(&ledger).expect("read");
        assert_eq!(cache.mark_session_verified(session).expect("mark"), 1);
        let after = std::fs::read_to_string(&ledger).expect("read");
        assert!(
            after.starts_with(&before),
            "the existing bytes must be untouched, with the update appended"
        );
        assert_eq!(after.lines().count(), 2);
    }
}
