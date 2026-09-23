//! Where generated code and the record of it live.
//!
//! Two things live under `<root>/.cargo-hole/`:
//!
//! - **Artifacts**: one generated `.rs` per source file that had holes,
//!   mirroring the source tree (`src/lib.rs` -> `.cargo-hole/patch/src/lib.rs`).
//!   They live in a `patch/` subdirectory so that the store's top level holds
//!   only the ledger and the lock, and so that generated code is never confused
//!   with `build/`, the disposable shadow tree that `cargo hole build` derives
//!   from it. The real source is never touched, so every hole keeps its
//!   `todo!("spec: ...")` and its byte offsets stay valid across runs.
//! - The **ledger**: `ledger.jsonl`, one JSON object per line, mapping a hole to
//!   the code that was generated for it.
//!
//! The ledger is what makes a repeated `fill` cheap. Because the source is
//! untouched, the same hole derives the same [`crate::hole::Hole::hole_key`] on
//! every run, so a second run replays the recorded answer instead of paying for
//! it again.
//!
//! # Why the ledger holds only three fields
//!
//! An entry is a cache slot, not a log. The whole question it answers is "what
//! code was proven to work for this hole", so it stores exactly that: which hole
//! ([`Entry::id`]), what code ([`Entry::code`]), and what format the record is
//! in ([`Entry::v`]).
//!
//! There is deliberately **no `verified` flag**. An earlier draft recorded every
//! attempt and marked it verified later, which needed a flag to tell the two
//! kinds apart and a session id to group a run's entries for a single
//! confirmation. That is the right shape for an audit log, but this is not one.
//! Instead, [`Storage::put`] is simply never called until the whole
//! generated tree has compiled -- so everything in the ledger is verified *by
//! construction*, and the field, the session, and the machinery that maintained
//! them all disappear.
//!
//! The cost is that this invariant is upheld by the caller rather than by the
//! type. The method name is the guard: a caller that records a candidate it
//! never compiled is doing something the name says it should not. Until
//! `src/verifier.rs` is written there is no compile gate to run, so today the
//! guard is only as good as the caller's restraint -- see [`Storage::put`].
//!
//! Nothing else is stored because nothing else is read. In particular the spec
//! and the function signature are not copied in: both are already committed to
//! by [`Entry::id`], and the spec still sits in the source, which is never
//! modified. A second copy could only drift out of step with the key that
//! actually decides replay, leaving a reader to guess which one is authoritative.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub mod disk;

use disk::DiskCache;

/// Directory holding artifacts and the ledger, relative to the crate root.
pub const STORE_DIR: &str = ".cargo-hole";

/// Name of the directory inside [`STORE_DIR`] that holds the generated code.
///
/// Kept separate from [`STORE_DIR`] itself so that the store's top level holds
/// only the ledger and the lock, and so that `build`'s shadow tree -- which is
/// derived, disposable output -- can never be confused with the generated code,
/// which is the product. Renaming this is a layout change: old artifacts left at
/// `.cargo-hole/src/` are not silently ignored, because ignoring them would build
/// an all-`todo!()` tree that still compiles.
pub const PATCH_DIR: &str = "patch";

/// Name of the append-only ledger inside [`STORE_DIR`].
pub const LEDGER_FILE: &str = "ledger.jsonl";

/// Schema version of the ledger.
///
/// Bump this whenever a field changes meaning, or a new field becomes
/// load-bearing for replay. An entry from another version is ignored rather than
/// read: regenerating it costs one model call, whereas misreading it could
/// splice the wrong code into a file.
pub const SCHEMA: u32 = 1;

/// One recorded answer: a hole and the code that was generated for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Ledger schema version; see [`SCHEMA`].
    pub v: u32,
    /// [`crate::hole::Hole::hole_key`] of the hole this is an answer for.
    pub id: String,
    /// The generated expression, exactly as spliced into the artifact.
    pub code: String,
}

impl Entry {
    /// An entry recording `code` as the answer for hole `id`.
    pub fn new(id: impl Into<String>, code: impl Into<String>) -> Entry {
        Entry {
            v: SCHEMA,
            id: id.into(),
            code: code.into(),
        }
    }
}

/// The store, behind an enum so callers never name a backend.
///
/// With a single variant this is speculative generality, and a plain struct
/// would do the same job today. It is kept because the shape is the point: an
/// in-memory backend (for `--check`, and for tests that must not touch a
/// filesystem) is the obvious next one, and adding it should not touch a single
/// call site.
#[derive(Debug)]
pub enum Storage {
    DiskCache(DiskCache),
}

impl Storage {
    /// Open the store rooted at `root`.
    ///
    /// Nothing is created here: `list` and `--check` legitimately run against a
    /// crate that has never been filled, and a read-only query should not leave
    /// a directory behind. The directory appears when something is written.
    pub fn open(root: &Path) -> Result<Storage> {
        Ok(Storage::DiskCache(DiskCache::open(root)?))
    }

    /// The directory artifacts and the ledger live in.
    pub fn dir(&self) -> &Path {
        match self {
            Storage::DiskCache(cache) => cache.dir(),
        }
    }

    /// Where the generated code for source file `file` belongs.
    ///
    /// Fails if `file` is outside the crate root, or if the relative path would
    /// climb out with `..`.
    pub fn artifact_path(&self, file: &Path) -> Result<PathBuf> {
        match self {
            Storage::DiskCache(cache) => cache.artifact_path(file),
        }
    }

    /// Write generated code for `file`, replacing any previous artifact.
    ///
    /// Returns the path written. The replacement is atomic, so an interrupted
    /// run cannot leave a half-written artifact that a later `--check` would
    /// read as real output.
    ///
    /// Writing an artifact records nothing in the ledger: an artifact may be
    /// rewritten many times before a version of it compiles. See
    /// [`Storage::put`].
    pub fn write_artifact(&self, file: &Path, src: &str) -> Result<PathBuf> {
        match self {
            Storage::DiskCache(cache) => cache.write_artifact(file, src),
        }
    }

    /// The recorded code for hole `id`, if the ledger has an answer for it.
    ///
    /// `None` is not an error and not a problem: it means this hole has to be
    /// generated, which is always correct, just slower.
    pub fn get(&self, id: &str) -> Option<&str> {
        match self {
            Storage::DiskCache(cache) => cache.get(id),
        }
    }

    /// Record `code` as the answer for hole `id`, appending to the ledger.
    ///
    /// **Call this only once the generated tree has compiled.** This method is
    /// where the ledger's one invariant is upheld: everything in it must be code
    /// that passed the compile gate. Recording a candidate that was never
    /// compiled caches it permanently, and every later run would replay it as
    /// though a gate had accepted it.
    ///
    /// The gate is per-tree, not per-hole, so record a run's entries after one
    /// `cargo check` of everything -- and if that check fails, record nothing. A
    /// hole's answer can depend on what another hole was filled with, so "this
    /// hole compiled" is not a property a single hole can have.
    ///
    /// Call this **after** [`Storage::write_artifact`], never before. See
    /// [`disk::DiskCache::put`] for why the order matters.
    pub fn put(&mut self, id: &str, code: &str) -> Result<()> {
        match self {
            Storage::DiskCache(cache) => cache.put(id, code),
        }
    }

    /// How many distinct holes the ledger has an answer for.
    pub fn len(&self) -> usize {
        match self {
            Storage::DiskCache(cache) => cache.len(),
        }
    }

    /// Whether the ledger has no answers at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hole::{Hole, HolePosition};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A directory unique per call, so parallel tests cannot collide.
    fn temp_root() -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "cargo-hole-storage-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("create the temp root");
        root
    }

    /// A hole with a fixed spec and signature, varying only where it sits.
    fn hole_at(spec: &str, line: usize) -> Hole {
        Hole {
            file: PathBuf::from("/tmp/crate/src/lib.rs"),
            byte_start: line * 10,
            byte_end: line * 10 + 20,
            spec: spec.to_string(),
            fn_sig: "pub fn double(n: i64) -> i64".to_string(),
            fn_name: "double".to_string(),
            impl_ctx: None,
            line,
            column: 5,
            macro_name: "todo".to_string(),
            position: HolePosition::Expression,
            pinned: false,
            unresolvable: None,
        }
    }

    #[test]
    fn a_fresh_crate_has_an_empty_ledger_and_no_directory() {
        let root = temp_root();
        let storage = Storage::open(&root).expect("open the store");
        assert!(storage.is_empty());
        assert!(storage.get("anything").is_none());
        assert!(
            !root.join(STORE_DIR).exists(),
            "reading must not create the store directory"
        );
    }

    #[test]
    fn a_recorded_answer_survives_reopening() {
        let root = temp_root();
        {
            let mut storage = Storage::open(&root).expect("open the store");
            storage.put("abc", "n * 2").expect("record");
        }
        let storage = Storage::open(&root).expect("reopen the store");
        assert_eq!(storage.get("abc"), Some("n * 2"));
    }

    #[test]
    fn an_unrecorded_hole_simply_misses() {
        // A miss is the normal state of a hole that has never been filled, and
        // must stay cheap and silent: it costs one generation, nothing more.
        let root = temp_root();
        let mut storage = Storage::open(&root).expect("open the store");
        storage.put("abc", "n * 2").expect("record");
        assert_eq!(storage.get("abc"), Some("n * 2"));
        assert_eq!(storage.get("def"), None);
    }

    #[test]
    fn the_newest_answer_for_a_hole_wins() {
        // Regenerating after a spec change appends; the newest entry answers.
        let root = temp_root();
        let mut storage = Storage::open(&root).expect("open the store");
        storage.put("abc", "n * 2").expect("record");
        storage.put("abc", "n * 3").expect("record");
        assert_eq!(
            storage.get("abc"),
            Some("n * 3"),
            "a regeneration must supersede the answer it replaced"
        );
    }

    #[test]
    fn recording_one_hole_does_not_disturb_another() {
        let root = temp_root();
        let mut storage = Storage::open(&root).expect("open the store");
        storage.put("a", "1").expect("record");
        storage.put("b", "2").expect("record");
        assert_eq!(storage.get("a"), Some("1"));
        assert_eq!(storage.get("b"), Some("2"));
        assert_eq!(storage.len(), 2);
    }

    #[test]
    fn a_corrupt_tail_does_not_lose_earlier_entries() {
        // A crash during an append can tear the last line. Everything before it
        // was complete when it was written, and must still be usable.
        let root = temp_root();
        {
            let mut storage = Storage::open(&root).expect("open the store");
            storage.put("a", "1").expect("record");
            storage.put("b", "2").expect("record");
        }
        let ledger = root.join(STORE_DIR).join(LEDGER_FILE);
        let mut text = std::fs::read_to_string(&ledger).expect("read the ledger");
        text.push_str("{\"v\":1,\"id\":\"c\",\"co");
        std::fs::write(&ledger, text).expect("tear the tail");

        let storage = Storage::open(&root).expect("reopen the store");
        assert_eq!(storage.len(), 2, "the intact entries survive");
        assert_eq!(storage.get("a"), Some("1"));
        assert_eq!(storage.get("c"), None);
    }

    #[test]
    fn an_unknown_schema_is_ignored_rather_than_misread() {
        let root = temp_root();
        let dir = root.join(STORE_DIR);
        std::fs::create_dir_all(&dir).expect("create the store dir");
        std::fs::write(
            dir.join(LEDGER_FILE),
            "{\"v\":99,\"id\":\"a\",\"code\":\"1\"}\n",
        )
        .expect("write the ledger");

        let storage = Storage::open(&root).expect("open the store");
        assert_eq!(
            storage.get("a"),
            None,
            "an entry from a schema we do not understand must not be replayed"
        );
    }

    #[test]
    fn an_entry_round_trips_through_json() {
        let entry = Entry::new("abc", "n * 2");
        let text = serde_json::to_string(&entry).expect("serialize");
        let back: Entry = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(back, entry);
        assert_eq!(back.v, SCHEMA);
    }

    #[test]
    fn a_line_missing_the_code_field_is_not_an_answer() {
        // Skipped, not read as an empty answer: splicing an empty string into a
        // hole would delete the hole instead of filling it.
        let root = temp_root();
        let dir = root.join(STORE_DIR);
        std::fs::create_dir_all(&dir).expect("create the store dir");
        std::fs::write(dir.join(LEDGER_FILE), "{\"v\":1,\"id\":\"a\"}\n").expect("write");

        let storage = Storage::open(&root).expect("open the store");
        assert_eq!(storage.get("a"), None);
    }

    // The ledger's correctness rests entirely on this: the key must follow the
    // hole's *meaning*, never its position. Line numbers drift under any edit
    // above the hole, and a key built from them would confidently serve one
    // hole's answer to another.
    #[test]
    fn the_key_ignores_where_the_hole_sits() {
        let first = Hole::hash_key(&hole_at("double the input", 10));
        let shifted = Hole::hash_key(&hole_at("double the input", 99));
        assert_eq!(first, shifted);

        let mut moved = hole_at("double the input", 10);
        moved.file = PathBuf::from("/tmp/crate/src/elsewhere.rs");
        moved.byte_start = 700;
        moved.byte_end = 720;
        assert_eq!(
            Hole::hash_key(&moved),
            first,
            "moving a function must not discard an answer that is still correct"
        );
    }

    #[test]
    fn the_key_ignores_spec_whitespace() {
        // `cargo fmt` re-wraps a multi-line spec, and that must not throw the
        // cache away.
        let wrapped = Hole::hash_key(&hole_at(
            "scale to fit within 1024px\n  on the longest side",
            10,
        ));
        let flat = Hole::hash_key(&hole_at(
            "scale to fit within 1024px on the longest side",
            10,
        ));
        assert_eq!(wrapped, flat);
    }

    #[test]
    fn the_key_changes_when_the_spec_changes() {
        let before = Hole::hash_key(&hole_at("double the input", 10));
        let after = Hole::hash_key(&hole_at("triple the input", 10));
        assert_ne!(before, after);
    }

    #[test]
    fn the_key_changes_when_the_signature_changes() {
        // The signature is the contract the answer has to satisfy: `i64` to
        // `u64` can invalidate an answer that used to compile.
        let mut changed = hole_at("double the input", 10);
        changed.fn_sig = "pub fn double(n: u64) -> u64".to_string();
        assert_ne!(
            Hole::hash_key(&changed),
            Hole::hash_key(&hole_at("double the input", 10))
        );
    }

    #[test]
    fn the_same_spec_in_two_functions_gets_two_keys() {
        // The same spec text in two functions is two holes, not one: sharing a
        // cache slot would let one answer overwrite the other.
        let mut other = hole_at("double the input", 10);
        other.fn_sig = "pub fn triple(n: i64) -> i64".to_string();
        assert_ne!(
            Hole::hash_key(&other),
            Hole::hash_key(&hole_at("double the input", 10))
        );
    }
}
