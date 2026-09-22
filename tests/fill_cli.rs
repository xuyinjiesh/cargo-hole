//! The `fill` command, driven end-to-end as the user runs it.
//!
//! These are the tests that caught a real bug: `fill` used to write its artifacts
//! *before* running the compile gate, so in `--in-place` mode a rejected answer
//! was left sitting in the user's source file. Driving the actual binary is the
//! only way to test that, because the ordering lives in `fill` itself.
//!
//! No model is needed. A hole is answered from the ledger, so a ledger written by
//! hand is enough to feed `fill` a chosen -- and chosen to be *wrong* -- answer.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

/// Where the test binary puts its scratch crates. Inside the crate's own target
/// directory, because `/tmp` is not reliably writable.
fn scratch_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/cli-tests")
}

/// The `cargo-hole` binary under test, as built alongside this test.
fn cargo_hole() -> PathBuf {
    let mut path = std::env::current_exe().expect("test exe");
    path.pop(); // deps/
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("cargo-hole")
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str, lib_rs: &str) -> Fixture {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = scratch_root().join(format!(
            "{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"clitest\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), lib_rs).unwrap();
        Fixture { dir }
    }

    fn lib(&self) -> PathBuf {
        self.dir.join("src/lib.rs")
    }

    fn ledger(&self) -> PathBuf {
        self.dir.join(".cargo-hole/ledger.jsonl")
    }

    fn source(&self) -> String {
        std::fs::read_to_string(self.lib()).unwrap()
    }

    /// Answer every hole in the crate from the ledger with `code`, bypassing the
    /// model. The key is what `list` reports, so it is read from the binary
    /// rather than reimplemented.
    fn seed_ledger(&self, code: &str) {
        let out = Command::new(cargo_hole())
            .args(["list", "--path"])
            .arg(&self.dir)
            .env("CARGO_HOLE_OFFLINE", "1")
            .output()
            .expect("run cargo-hole list");
        assert!(
            out.status.success(),
            "list failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // `list --pretty` is not needed: the plain layout is one line per hole.
        // The key is a blake3 digest of the hole's meaning, which `list` does not
        // print, so ask the library instead -- but `list` is what proves the
        // crate is readable, and its output pins the hole count.
        let listed = String::from_utf8_lossy(&out.stdout);
        let count = listed.matches("spec:").count();
        assert!(count > 0, "no holes found:\n{listed}");

        // Recompute the keys the same way the tool does, via its own public API.
        let holes = cargo_hole::hole::Hole::list_holes(&self.dir).expect("list holes");
        assert_eq!(holes.len(), count, "list and the library disagree");

        std::fs::create_dir_all(self.dir.join(".cargo-hole")).unwrap();
        let mut text = String::new();
        for h in &holes {
            text.push_str(&format!(
                "{{\"v\":{},\"id\":\"{}\",\"code\":{}}}\n",
                1,
                h.hash_key(),
                serde_json::to_string(code).unwrap()
            ));
        }
        std::fs::write(self.ledger(), text).unwrap();
    }

    /// Run `fill` with the given extra arguments.
    fn fill(&self, extra: &[&str]) -> std::process::Output {
        Command::new(cargo_hole())
            .arg("fill")
            .arg("--path")
            .arg(&self.dir)
            .args(extra)
            .env("CARGO_HOLE_OFFLINE", "1")
            // Keep the child's cargo from fighting the outer one for the lock.
            .env("CARGO_TARGET_DIR", self.dir.join("target"))
            .env(
                "CARGO_HOME",
                Path::new(env!("CARGO_MANIFEST_DIR")).join(".cargo"),
            )
            .output()
            .expect("run cargo-hole fill")
    }
}

/// The bug this test file exists for: a rejected answer must never be left in the
/// user's source, and `--in-place` is the mode where that is destructive.
#[test]
fn a_rejected_answer_is_never_written_in_place() {
    let fx = Fixture::new(
        "inplace",
        "pub fn answer() -> i64 {\n    todo!(\"spec: return 42\")\n}\n",
    );
    let before = fx.source();
    // An answer that does not compile.
    fx.seed_ledger("missing_ident");

    let out = fx.fill(&["--in-place"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert_eq!(
        fx.source(),
        before,
        "the source must be exactly as the user wrote it.\nstderr:\n{stderr}"
    );
    assert!(
        !fx.source().contains("missing_ident"),
        "the rejected answer leaked into the source"
    );
    assert!(
        !fx.lib().with_extension("rs.cargo-hole.bak").exists(),
        "a backup was left behind"
    );
    // The gate ran and rejected, so the run must say so.
    assert!(
        stderr.contains("did not compile") || stderr.contains("still cannot fill"),
        "the rejection was not reported:\n{stderr}"
    );
}

/// Without `--in-place`, artifacts go to `.cargo-hole/`, so the source was never
/// at risk -- but the artifact itself must not be written either, or a later run
/// could read it back as real output.
#[test]
fn a_rejected_answer_is_never_written_as_an_artifact() {
    let fx = Fixture::new(
        "artifact",
        "pub fn answer() -> i64 {\n    todo!(\"spec: return 42\")\n}\n",
    );
    fx.seed_ledger("missing_ident");

    let out = fx.fill(&[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() || !stderr.is_empty(),
        "unexpected: {stderr}"
    );

    let artifact = fx.dir.join(".cargo-hole/src/lib.rs");
    if artifact.exists() {
        let text = std::fs::read_to_string(&artifact).unwrap();
        assert!(
            !text.contains("missing_ident"),
            "a rejected answer reached the artifact:\n{text}"
        );
    }
    assert!(!fx.source().contains("missing_ident"), "source touched");
}

/// A correct answer still gets written, and recorded -- otherwise the gate would
/// be a very elaborate way of doing nothing.
#[test]
fn an_accepted_answer_is_written_and_recorded() {
    let fx = Fixture::new(
        "accept",
        "pub fn answer() -> i64 {\n    todo!(\"spec: return 42\")\n}\n",
    );
    fx.seed_ledger("42");

    let out = fx.fill(&["--in-place"]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(
        fx.source(),
        "pub fn answer() -> i64 {\n    42\n}\n",
        "the accepted answer should be in place.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !fx.lib().with_extension("rs.cargo-hole.bak").exists(),
        "a backup survived an accepted run"
    );
}

/// `--no-verify` must write no ledger entry. It is the one flag that turns the
/// gate off, so it is the one path where an unverified answer could reach the
/// cache.
#[test]
fn no_verify_writes_no_ledger_entry() {
    let fx = Fixture::new(
        "noverify",
        "pub fn answer() -> i64 {\n    todo!(\"spec: return 42\")\n}\n",
    );
    // Nothing in the ledger, and no model available, so the hole cannot be filled
    // -- but the ledger must not be created even so.
    let out = fx.fill(&["--no-verify", "--in-place"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        !fx.ledger().exists(),
        "--no-verify must not create a ledger.\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("--no-verify"),
        "the skipped gate was not reported:\n{stderr}"
    );
    assert!(
        !fx.source().contains("missing_ident"),
        "source changed with nothing to fill"
    );
}

/// A crate that was already broken is reported as such, and no answer is blamed.
#[test]
fn an_already_broken_crate_blames_no_answer() {
    let fx = Fixture::new(
        "broken",
        "pub fn broken() -> i64 {\n    \"not an i64\"\n}\n\n\
         pub fn answer() -> i64 {\n    todo!(\"spec: return 42\")\n}\n",
    );
    fx.seed_ledger("42");

    let out = fx.fill(&["--in-place"]);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.contains("already broken"),
        "a pre-existing error should be named as such:\n{stderr}"
    );
    assert!(
        fx.source().contains("todo!(\"spec: return 42\")"),
        "the correct answer must not be written while the crate is broken:\n{stderr}"
    );
}
