//! End-to-end behaviour of the compile gate, exercised through the same public
//! API `fill` uses.
//!
//! These tests exist because the gate's most important property is invisible to
//! a unit test of any single function: it is that a *bad* answer never reaches
//! the ledger. That requires a real crate, a real `cargo check`, and a ledger on
//! disk.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use cargo_hole::hole::Hole;
use cargo_hole::prober::ProberOptions;
use cargo_hole::storage::Storage;
use cargo_hole::verifier::{Patch, Verdict, Verifier};

/// A throwaway crate on disk.
struct TempCrate {
    dir: PathBuf,
}

impl TempCrate {
    fn new(tag: &str, lib_rs: &str) -> TempCrate {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cargo-hole-e2e-{tag}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"e2ecrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/lib.rs"), lib_rs).unwrap();
        TempCrate { dir }
    }

    fn lib(&self) -> PathBuf {
        self.dir.join("src/lib.rs")
    }

    fn holes(&self) -> Vec<Hole> {
        Hole::list_holes_one_file(&self.lib()).expect("extract holes")
    }

    fn options(&self) -> ProberOptions {
        let cargo_home = std::env::temp_dir().join("cargo-hole-cargo-home");
        let _ = std::fs::create_dir_all(&cargo_home);
        ProberOptions::for_tests(self.dir.join("target"), cargo_home)
    }

    fn verifier(&self) -> Verifier {
        Verifier::new(self.dir.clone(), self.options())
    }

    /// The patch a `fill` would hand the gate for these answers.
    fn patch(&self, answers: &[Option<&str>]) -> Patch {
        let src = std::fs::read_to_string(self.lib()).unwrap();
        let holes = self.holes();
        assert_eq!(holes.len(), answers.len(), "one answer per hole");
        let pairs: Vec<(&Hole, Option<&str>)> =
            holes.iter().zip(answers).map(|(h, a)| (h, *a)).collect();
        Patch::splice(self.lib(), &src, &pairs).expect("splice")
    }
}

/// The gate must never let a bad answer become a ledger entry.
///
/// This is the property the whole module exists for, and the only way to test it
/// is to actually record something and look at the ledger afterwards.
#[test]
fn a_rejected_answer_is_never_recorded() {
    let tc = TempCrate::new(
        "ledger",
        "pub fn f() -> i64 {\n    todo!(\"spec: answer\")\n}\n",
    );
    let hole = tc.holes().remove(0);

    // A wrong answer, as a model might return it.
    let verdict = tc.verifier().verify(&[tc.patch(&[Some("\"not an i64\"")])]);
    assert!(verdict.is_rejected(), "{verdict:?}");

    // The gate rejected it, so `fill` must not have recorded it. Modelled here
    // the way `fill` does it: record only on acceptance.
    if verdict.is_accepted() {
        let mut storage = Storage::open(&tc.dir).unwrap();
        storage.put(&hole.hash_key(), "\"not an i64\"").unwrap();
    }

    let storage = Storage::open(&tc.dir).unwrap();
    assert!(
        storage.get(&hole.hash_key()).is_none(),
        "a rejected answer must not be replayable"
    );

    // And the correct answer does get recorded, so the gate is not simply
    // refusing everything.
    let verdict = tc.verifier().verify(&[tc.patch(&[Some("42")])]);
    assert!(verdict.is_accepted(), "{verdict:?}");
    let mut storage = Storage::open(&tc.dir).unwrap();
    storage.put(&hole.hash_key(), "42").unwrap();
    let storage = Storage::open(&tc.dir).unwrap();
    assert_eq!(storage.get(&hole.hash_key()), Some("42"));
}

/// A poisoned ledger must be caught, not replayed.
///
/// This models the real hazard: an entry written before the gate existed, or by
/// a run that was interrupted. Replaying it writes broken code into the user's
/// source, so the gate has to reject the tree it produces.
#[test]
fn a_poisoned_ledger_entry_is_caught_by_the_gate() {
    let tc = TempCrate::new(
        "poison",
        "pub fn f() -> i64 {\n    todo!(\"spec: answer\")\n}\n",
    );
    let hole = tc.holes().remove(0);

    {
        let mut storage = Storage::open(&tc.dir).unwrap();
        storage.put(&hole.hash_key(), "missing_ident").unwrap();
    }
    let storage = Storage::open(&tc.dir).unwrap();
    let replayed = storage.get(&hole.hash_key()).expect("the poison is in");
    assert_eq!(replayed, "missing_ident");

    // `fill` splices whatever the ledger gave it, then gates the result.
    let verdict = tc.verifier().verify(&[tc.patch(&[Some(replayed)])]);
    assert!(
        verdict.is_rejected(),
        "the gate must reject a tree built from a bad ledger entry: {verdict:?}"
    );
    assert_eq!(
        verdict.attributable().len(),
        1,
        "and it must name the hole responsible: {verdict:?}"
    );
    assert_eq!(
        verdict.attributable()[0].hole.as_ref().unwrap().spec,
        "answer"
    );
}

/// The gate leaves the real source exactly as it found it, whatever it decides.
///
/// It has to patch the real files to check them, so a failure to restore would
/// destroy the user's source -- the one outcome worse than a bad cache entry.
#[test]
fn the_source_survives_every_verdict() {
    let tc = TempCrate::new(
        "survive",
        "pub fn a() -> i64 {\n    todo!(\"spec: a\")\n}\n\n\
         pub fn b() -> bool {\n    todo!(\"spec: b\")\n}\n",
    );
    let before = std::fs::read(tc.lib()).unwrap();

    let good = tc
        .verifier()
        .verify(&[tc.patch(&[Some("1"), Some("true")])]);
    assert!(good.is_accepted(), "{good:?}");
    assert_eq!(std::fs::read(tc.lib()).unwrap(), before, "after acceptance");

    let bad = tc
        .verifier()
        .verify(&[tc.patch(&[Some("1"), Some("1 + 1")])]);
    assert!(bad.is_rejected(), "{bad:?}");
    assert_eq!(std::fs::read(tc.lib()).unwrap(), before, "after rejection");

    assert!(
        !tc.dir.join("src/lib.rs.cargo-hole.bak").exists(),
        "no backup may survive either outcome"
    );
}

/// A partial fill still has to compile: unfilled holes stay `todo!()`, which is
/// valid, so they must not fail the gate.
///
/// This matters because a pinned or unfillable hole is a normal outcome, and
/// failing the whole tree for it would make the gate unusable on any crate that
/// has one.
#[test]
fn unfilled_holes_do_not_fail_the_gate() {
    let tc = TempCrate::new(
        "partial",
        "pub fn a() -> i64 {\n    todo!(\"spec: a\")\n}\n\n\
         pub fn b() -> bool {\n    todo!(\"spec: b\")\n}\n",
    );

    let verdict = tc.verifier().verify(&[tc.patch(&[Some("1"), None])]);
    assert!(
        verdict.is_accepted(),
        "an unelaborated hole compiles, so it must not fail the gate: {verdict:?}"
    );

    // But a *wrong* answer next to an unfilled hole is still caught.
    let verdict = tc.verifier().verify(&[tc.patch(&[Some("\"nope\""), None])]);
    assert!(verdict.is_rejected(), "{verdict:?}");
    assert_eq!(verdict.attributable()[0].hole.as_ref().unwrap().spec, "a");
}

/// The gate blames the hole that is actually wrong, not its neighbour.
///
/// Attribution is what makes a rejection actionable: without it every failure
/// re-asks for every answer.
#[test]
fn the_gate_blames_the_wrong_answer_only() {
    let tc = TempCrate::new(
        "blame",
        "pub fn a() -> i64 {\n    todo!(\"spec: a\")\n}\n\n\
         pub fn b() -> bool {\n    todo!(\"spec: b\")\n}\n\n\
         pub fn c() -> String {\n    todo!(\"spec: c\")\n}\n",
    );

    // `a` and `c` are right, `b` is not.
    let verdict = tc
        .verifier()
        .verify(&[tc.patch(&[Some("7"), Some("7"), Some("String::new()")])]);

    assert!(verdict.is_rejected(), "{verdict:?}");
    let blamed = verdict.attributable();
    assert_eq!(
        blamed.len(),
        1,
        "exactly one answer is at fault: {verdict:?}"
    );
    assert_eq!(blamed[0].hole.as_ref().unwrap().spec, "b");
    assert!(verdict.pre_existing().is_empty(), "{verdict:?}");
}

/// Two files are checked as one tree, in a single gate.
///
/// A bad answer in either file must fail the tree: a hole's answer can depend on
/// another file's answer, so no file can be accepted on its own.
#[test]
fn a_bad_answer_in_any_file_fails_the_whole_tree() {
    let tc = TempCrate::new("twofiles", "pub mod a;\npub mod b;\n");
    let a = tc.dir.join("src/a.rs");
    let b = tc.dir.join("src/b.rs");
    std::fs::write(
        &a,
        "pub fn value() -> i64 {\n    todo!(\"spec: value\")\n}\n",
    )
    .unwrap();
    std::fs::write(
        &b,
        "pub fn flag() -> bool {\n    todo!(\"spec: flag\")\n}\n",
    )
    .unwrap();

    let before_a = std::fs::read(&a).unwrap();
    let before_b = std::fs::read(&b).unwrap();

    let patch_for = |file: &Path, spec: &str, answer: &str| {
        let src = std::fs::read_to_string(file).unwrap();
        let holes = Hole::list_holes_one_file(file).unwrap();
        let hole = holes.iter().find(|h| h.spec == spec).expect("the hole");
        Patch::splice(file, &src, &[(hole, Some(answer))]).expect("splice")
    };

    // `a` is fine, `b` is not.
    let verdict = tc
        .verifier()
        .verify(&[patch_for(&a, "value", "1"), patch_for(&b, "flag", "1 + 1")]);

    assert!(verdict.is_rejected(), "{verdict:?}");
    let blamed = verdict.attributable();
    assert_eq!(blamed.len(), 1, "{verdict:?}");
    assert_eq!(blamed[0].hole.as_ref().unwrap().spec, "flag");

    assert_eq!(std::fs::read(&a).unwrap(), before_a, "a.rs restored");
    assert_eq!(std::fs::read(&b).unwrap(), before_b, "b.rs restored");
    assert!(!a.with_extension("rs.cargo-hole.bak").exists());
    assert!(!b.with_extension("rs.cargo-hole.bak").exists());
}

/// A crate that was already broken is reported without blaming any answer.
#[test]
fn an_already_broken_crate_is_not_blamed_on_the_model() {
    let tc = TempCrate::new(
        "already-broken",
        "pub fn broken() -> i64 {\n    \"pre-existing\"\n}\n\n\
         pub fn f() -> i64 {\n    todo!(\"spec: answer\")\n}\n",
    );

    let verdict = tc.verifier().verify(&[tc.patch(&[Some("42")])]);

    assert!(verdict.is_rejected(), "{verdict:?}");
    assert!(
        verdict.attributable().is_empty(),
        "the answer was correct: {verdict:?}"
    );
    assert_eq!(verdict.pre_existing().len(), 1, "{verdict:?}");
    assert!(
        verdict.label().contains("before this run"),
        "{}",
        verdict.label()
    );
}

/// A rejected tree reports every verdict as a rejection, never as acceptance.
#[test]
fn verdict_helpers_agree_with_the_variant() {
    let accepted = Verdict::Accepted;
    assert!(accepted.is_accepted() && !accepted.is_rejected());
    assert!(accepted.attributable().is_empty());
    assert_eq!(accepted.label(), "verified");

    let inconclusive = Verdict::Inconclusive {
        reason: "cargo is not installed".to_string(),
    };
    assert!(!inconclusive.is_accepted() && !inconclusive.is_rejected());
    assert!(inconclusive.label().contains("not verified"));
}
