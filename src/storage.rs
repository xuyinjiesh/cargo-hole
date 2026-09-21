//! Where generated code and the record of it live.
//!
//! Two things live under `<root>/.cargo-hole/`:
//!
//! - **Artifacts**: one generated `.rs` per source file that had holes,
//!   mirroring the source tree (`src/lib.rs` -> `.cargo-hole/src/lib.rs`). The
//!   real source is never touched, so every hole keeps its `todo!("spec: ...")`
//!   and its byte offsets stay valid across runs.
//! - The **ledger**: `ledger.jsonl`, one JSON object per line, recording what
//!   was generated for which hole and whether it was verified.
//!
//! The ledger is what makes a repeated `fill` cheap. Because the source is
//! untouched, the same hole derives the same [`crate::hole::Hole::hole_key`] on
//! every run, so a second run replays the recorded answer instead of paying for
//! it again -- but only when that answer was verified. See
//! [`Storage::lookup_verified`] for why that qualifier is not negotiable.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub mod disk;

use disk::DiskCache;

/// Directory holding artifacts and the ledger, relative to the crate root.
pub const STORE_DIR: &str = ".cargo-hole";

/// Name of the append-only ledger inside [`STORE_DIR`].
pub const LEDGER_FILE: &str = "ledger.jsonl";

/// Schema version of the ledger.
///
/// Bump this whenever an existing field changes meaning, or a new field becomes
/// load-bearing for replay. Entries from an older schema are ignored: they are
/// cheap to regenerate, and reusing one under different semantics would be a
/// guess dressed up as a cache hit.
pub const SCHEMA: u32 = 1;

/// One recorded hole fill.
///
/// The ledger deliberately does **not** copy the spec or the function
/// signature. Both are already committed to by [`Entry::id`], and the spec still
/// sits in the source, which is never modified. A second copy could only drift
/// out of step with the key that actually decides replay, leaving a reader to
/// work out which one is authoritative.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Ledger schema version; see [`SCHEMA`].
    pub v: u32,
    /// [`crate::hole::Hole::hole_key`] of the hole this is an answer for.
    pub id: String,
    /// The generated expression, exactly as spliced into the artifact.
    pub code: String,
    /// `Agent::label()` of whatever produced it, so a model change is visible
    /// both in review and in the ledger's history.
    pub agent: String,
    /// Whether the artifact containing this code passed the compile gate.
    pub verified: bool,
    /// Shared by every entry one `fill` run writes; see [`new_session`].
    pub session: u64,
    /// Unix seconds, for `--check` reporting and for ordering by eye.
    pub at: u64,
}

impl Entry {
    /// A new entry for `id`, stamped with the current time.
    ///
    /// Born unverified: only a compile gate may set that flag, so an entry
    /// cannot start life claiming a check that never ran.
    pub fn new(id: impl Into<String>, code: impl Into<String>, agent: impl Into<String>) -> Entry {
        Entry {
            v: SCHEMA,
            id: id.into(),
            code: code.into(),
            agent: agent.into(),
            verified: false,
            session: new_session(),
            at: now_unix(),
        }
    }
}

/// A session id shared by every entry a single run writes.
///
/// The run is the unit of verification, not the hole: whether hole A's answer
/// compiles can depend on what hole B was filled with, so "this hole is
/// verified" is only meaningful together with the rest of the tree. The session
/// id is how a caller groups the entries it must confirm or discard as a whole.
///
/// Two runs must never share one. If they did, a half-filled tree from one run
/// could be confirmed by another run's compile check, laundering unverified code
/// into the cache.
pub fn new_session() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    // Mix in the pid: two processes started inside the same nanosecond are
    // otherwise indistinguishable, and sessions must not collide across
    // processes. The constant is the usual 64-bit odd multiplier, which spreads
    // a small pid across the whole word.
    nanos ^ (std::process::id() as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// Seconds since the Unix epoch, or 0 if the clock is set before it.
fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    pub fn write_artifact(&self, file: &Path, src: &str) -> Result<PathBuf> {
        match self {
            Storage::DiskCache(cache) => cache.write_artifact(file, src),
        }
    }

    /// The most recent entry for `id`, verified or not.
    ///
    /// For reporting. Do not replay from this: see [`Storage::lookup_verified`].
    pub fn lookup(&self, id: &str) -> Option<&Entry> {
        match self {
            Storage::DiskCache(cache) => cache.lookup(id),
        }
    }

    /// The most recent **verified** entry for `id`, if there is one.
    ///
    /// This is the only thing worth replaying. An unverified entry records an
    /// answer no compile gate ever accepted -- typically because `--no-verify`
    /// was used -- and serving it would launder that answer into the cache,
    /// where it is indistinguishable from verified output and would then be
    /// replayed forever.
    ///
    /// A missing verified entry is not an error: it costs one generation, which
    /// is always correct.
    pub fn lookup_verified(&self, id: &str) -> Option<&Entry> {
        match self {
            Storage::DiskCache(cache) => cache.lookup_verified(id),
        }
    }

    /// Append `entry` to the ledger and remember it.
    ///
    /// Call this **after** the artifact is on disk, never before: see
    /// [`disk::DiskCache::record`].
    pub fn record(&mut self, entry: Entry) -> Result<()> {
        match self {
            Storage::DiskCache(cache) => cache.record(entry),
        }
    }

    /// Mark every entry from `session` as verified, and persist that.
    ///
    /// Called once, after the whole generated tree has compiled. Verifying per
    /// hole would be meaningless, since one hole's answer can depend on
    /// another's.
    pub fn mark_session_verified(&mut self, session: u64) -> Result<usize> {
        match self {
            Storage::DiskCache(cache) => cache.mark_session_verified(session),
        }
    }

    /// How many holes the ledger knows about, verified or not.
    pub fn len(&self) -> usize {
        match self {
            Storage::DiskCache(cache) => cache.len(),
        }
    }

    /// Whether the ledger knows about any hole at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How many entries are replayable, for the `N replayed` line.
    pub fn verified_count(&self) -> usize {
        match self {
            Storage::DiskCache(cache) => cache.verified_count(),
        }
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

    /// A hole with a fixed spec, varying only where it sits.
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
        assert_eq!(storage.verified_count(), 0);
        assert!(storage.lookup("anything").is_none());
        assert!(
            !root.join(STORE_DIR).exists(),
            "reading must not create the store directory"
        );
    }

    #[test]
    fn a_recorded_entry_survives_reopening() {
        let root = temp_root();
        {
            let mut storage = Storage::open(&root).expect("open the store");
            let mut entry = Entry::new("abc", "n * 2", "cli:codex:gpt-5");
            entry.verified = true;
            storage.record(entry).expect("record");
        }
        let storage = Storage::open(&root).expect("reopen the store");
        let entry = storage.lookup_verified("abc").expect("replayed");
        assert_eq!(entry.code, "n * 2");
        assert_eq!(entry.agent, "cli:codex:gpt-5");
    }

    #[test]
    fn an_unverified_entry_is_never_replayed() {
        // The `--no-verify` case: recorded and reported, but not reusable.
        let root = temp_root();
        let mut storage = Storage::open(&root).expect("open the store");
        let mut entry = Entry::new("abc", "n + 1", "cli:codex:gpt-5");
        entry.verified = false;
        storage.record(entry).expect("record");

        assert!(storage.lookup("abc").is_some(), "still worth reporting");
        assert!(
            storage.lookup_verified("abc").is_none(),
            "unverified code must never be served as if a gate had accepted it"
        );
        assert_eq!(storage.verified_count(), 0);
    }

    #[test]
    fn the_newest_entry_for_a_hole_wins() {
        let root = temp_root();
        let mut storage = Storage::open(&root).expect("open the store");
        for code in ["first", "second"] {
            let mut entry = Entry::new("abc", code, "cli:codex:gpt-5");
            entry.verified = true;
            storage.record(entry).expect("record");
        }
        assert_eq!(
            storage.lookup("abc").expect("found").code,
            "second",
            "a regeneration must supersede the answer it replaced"
        );
    }

    #[test]
    fn a_later_rejection_leaves_the_earlier_verified_answer_intact() {
        // A rejected candidate is never written to disk, so the artifact still
        // holds the verified answer. The older entry is therefore the one that
        // matches what is actually on disk, and replaying it is correct.
        let root = temp_root();
        let mut storage = Storage::open(&root).expect("open the store");
        let mut good = Entry::new("abc", "n * 2", "cli:codex:gpt-5");
        good.verified = true;
        storage.record(good).expect("record");
        storage
            .record(Entry::new("abc", "n + 1", "cli:codex:gpt-5"))
            .expect("record");

        assert_eq!(storage.lookup("abc").expect("found").code, "n + 1");
        assert_eq!(
            storage.lookup_verified("abc").expect("replayed").code,
            "n * 2"
        );
    }

    #[test]
    fn a_session_is_confirmed_as_a_whole() {
        let root = temp_root();
        let mut storage = Storage::open(&root).expect("open the store");
        let session = new_session();
        for (id, code) in [("a", "1"), ("b", "2")] {
            let mut entry = Entry::new(id, code, "cli:codex:gpt-5");
            entry.session = session;
            storage.record(entry).expect("record");
        }
        assert_eq!(storage.verified_count(), 0, "nothing is verified yet");

        let marked = storage.mark_session_verified(session).expect("mark");
        assert_eq!(marked, 2, "the whole run is confirmed at once");
        assert_eq!(storage.verified_count(), 2);
        assert_eq!(storage.lookup_verified("a").expect("a").code, "1");
        assert_eq!(storage.lookup_verified("b").expect("b").code, "2");
    }

    #[test]
    fn a_failed_run_marks_nothing() {
        // The gate failed, so the entries stay unverified even though they were
        // recorded. This is the difference between "we wrote it" and "we proved
        // it", and the cache must respect it.
        let root = temp_root();
        let mut storage = Storage::open(&root).expect("open the store");
        let session = new_session();
        let mut entry = Entry::new("a", "n + 1", "cli:codex:gpt-5");
        entry.session = session;
        storage.record(entry).expect("record");

        assert_eq!(storage.verified_count(), 0);
        assert!(storage.lookup_verified("a").is_none());
    }

    #[test]
    fn marking_one_session_leaves_other_sessions_alone() {
        let root = temp_root();
        let mut storage = Storage::open(&root).expect("open the store");
        let old = new_session();
        let new = new_session();
        let mut a = Entry::new("a", "1", "cli:codex:gpt-5");
        a.session = old;
        storage.record(a).expect("record");
        let mut b = Entry::new("b", "2", "cli:codex:gpt-5");
        b.session = new;
        storage.record(b).expect("record");

        assert_eq!(storage.mark_session_verified(new).expect("mark"), 1);
        assert!(
            storage.lookup_verified("a").is_none(),
            "the old run stays out"
        );
        assert_eq!(storage.lookup_verified("b").expect("b").code, "2");
    }

    #[test]
    fn a_corrupt_tail_does_not_lose_earlier_entries() {
        // A crash during an append can tear the last line. Everything before it
        // was complete when it was written, and must still be usable.
        let root = temp_root();
        {
            let mut storage = Storage::open(&root).expect("open the store");
            let mut first = Entry::new("a", "1", "cli:codex:gpt-5");
            first.verified = true;
            storage.record(first).expect("record");
            let mut second = Entry::new("b", "2", "cli:codex:gpt-5");
            second.verified = true;
            storage.record(second).expect("record");
        }
        let ledger = root.join(STORE_DIR).join(LEDGER_FILE);
        let mut text = std::fs::read_to_string(&ledger).expect("read the ledger");
        text.push_str("{\"v\":1,\"id\":\"c\",\"co");
        std::fs::write(&ledger, text).expect("tear the tail");

        let storage = Storage::open(&root).expect("reopen the store");
        assert_eq!(storage.verified_count(), 2, "the intact entries survive");
        assert_eq!(storage.lookup_verified("a").expect("a").code, "1");
        assert!(storage.lookup_verified("c").is_none());
    }

    #[test]
    fn an_unknown_schema_is_ignored_rather_than_misread() {
        let root = temp_root();
        let dir = root.join(STORE_DIR);
        std::fs::create_dir_all(&dir).expect("create the store dir");
        std::fs::write(
            dir.join(LEDGER_FILE),
            "{\"v\":99,\"id\":\"a\",\"code\":\"1\",\"agent\":\"x\",\"verified\":true,\"session\":1,\"at\":1}\n",
        )
        .expect("write the ledger");

        let storage = Storage::open(&root).expect("open the store");
        assert!(
            storage.lookup_verified("a").is_none(),
            "an entry from a schema we do not understand must not be replayed"
        );
    }

    // The ledger's correctness rests entirely on this: the key must follow the
    // hole's *meaning*, never its position. Line numbers drift under any edit
    // above the hole, and a key built from them would confidently serve one
    // hole's answer to another.
    #[test]
    fn the_key_ignores_where_the_hole_sits() {
        let first = Hole::hole_key(&hole_at("double the input", 10));
        let shifted = Hole::hole_key(&hole_at("double the input", 99));
        assert_eq!(first, shifted);

        let mut moved = hole_at("double the input", 10);
        moved.file = PathBuf::from("/tmp/crate/src/elsewhere.rs");
        moved.byte_start = 700;
        moved.byte_end = 720;
        assert_eq!(
            Hole::hole_key(&moved),
            first,
            "moving a function must not discard an answer that is still correct"
        );
    }

    #[test]
    fn the_key_ignores_spec_whitespace() {
        // `cargo fmt` re-wraps a multi-line spec, and that must not throw the
        // cache away.
        let wrapped = Hole::hole_key(&hole_at(
            "scale to fit within 1024px\n  on the longest side",
            10,
        ));
        let flat = Hole::hole_key(&hole_at(
            "scale to fit within 1024px on the longest side",
            10,
        ));
        assert_eq!(wrapped, flat);
    }

    #[test]
    fn the_key_changes_when_the_spec_changes() {
        let before = Hole::hole_key(&hole_at("double the input", 10));
        let after = Hole::hole_key(&hole_at("triple the input", 10));
        assert_ne!(before, after);
    }

    #[test]
    fn the_key_changes_when_the_signature_changes() {
        // The signature is the contract the answer has to satisfy: `i64` to
        // `u64` can invalidate an answer that used to compile.
        let mut changed = hole_at("double the input", 10);
        changed.fn_sig = "pub fn double(n: u64) -> u64".to_string();
        assert_ne!(
            Hole::hole_key(&changed),
            Hole::hole_key(&hole_at("double the input", 10))
        );
    }

    #[test]
    fn sessions_do_not_collide() {
        // Not a statistical test of the mixing, just a guard against a constant
        // being returned, which would defeat per-run grouping entirely.
        assert_ne!(new_session(), new_session());
    }
}
