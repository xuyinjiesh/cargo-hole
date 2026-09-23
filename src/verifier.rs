//! The compile gate: the one thing that makes the ledger trustworthy.
//!
//! [`crate::storage`] records an answer for a hole only once the generated tree
//! has compiled. This module decides that. Without it, the ledger's central
//! claim -- "everything in here is code the gate accepted" -- is upheld by the
//! caller's restraint alone.
//!
//! # Why a naive `cargo check` would be a silent no-op
//!
//! The obvious implementation -- run `cargo check` in the crate root -- never
//! fails, and would accept anything.
//!
//! The reason is the trick the whole tool rests on. A hole is `todo!("spec:
//! ...")`, whose type is `!`, which coerces to every type, so a crate full of
//! unelaborated holes compiles cleanly. In the default (non `--in-place`) mode
//! `fill` writes its answers to `<root>/.cargo-hole/patch/src/lib.rs` and never touches
//! the real source, so a check of the crate root compiles the *original*,
//! still-`todo!()` tree, and passes no matter what the model returned.
//!
//! A gate must therefore put the generated code into the compilation unit before
//! checking it. That is what [`Verifier::verify`] does: it patches every file in
//! place, runs one check, and restores every file byte-for-byte.
//!
//! This is the same patch-check-restore manoeuvre [`crate::prober`] performs, and
//! it reuses the same machinery -- [`crate::prober::RestoreGuard`], so a crash
//! leaves recoverable `.bak` files, and [`crate::prober::ProbeLock`] -- rather
//! than inventing a second one.
//!
//! # Every file at once, and the gate is per tree
//!
//! Two things follow from the above, and both are easy to get wrong:
//!
//! - **All files are patched simultaneously**, then one check runs. Checking one
//!   file at a time would compile the *other* files in their unfilled state,
//!   where every hole is a `todo!()` that coerces to anything. A file whose
//!   answer depends on another file's answer would then be checked without it,
//!   and pass for the wrong reason. This is the naive-root mistake at a smaller
//!   scale.
//! - **The verdict is per tree, not per hole.** [`crate::storage::Storage::put`]
//!   spells out why: a hole's answer can depend on what another hole was filled
//!   with, so "this hole compiled" is not a property a single hole can have.
//!   Hence one check for the whole tree, and [`Verdict::Accepted`] or nothing.
//!
//! # What a rejection reports
//!
//! A rejection is only useful if it says *which* answers are at fault, because
//! the caller can then re-ask for those and keep the rest. That attribution
//! happens here: every error is mapped to the hole whose generated code it lands
//! in, using the byte ranges in the checked text that
//! [`crate::prober::splice_many`] hands back.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::hole::Hole;
use crate::prober::{
    Diagnostic, Edit, ProbeLock, Prober, ProberOptions, RestoreGuard, same_file, splice_many,
};

/// Where one hole's generated code ended up in the checked text.
///
/// A byte range rather than a copy of the code, because the only question asked
/// of a region is which diagnostics land inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub hole: Hole,
    /// Start of the generated code in the file's checked text.
    pub byte_start: usize,
    /// End of it, exclusive.
    pub byte_end: usize,
}

impl Region {
    pub fn contains(&self, byte: usize) -> bool {
        self.byte_start <= byte && byte <= self.byte_end
    }
}

/// One file to check: the path on disk, the text that should be there while the
/// check runs, and where each generated answer sits in that text.
///
/// `checked` is the artifact `fill` just produced, which may hold answers
/// replayed from the ledger as well as freshly generated ones. Both are in the
/// check, because old and new answers can depend on each other: verifying only
/// the new ones would not verify anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    pub file: PathBuf,
    pub checked: String,
    pub regions: Vec<Region>,
}

impl Patch {
    pub fn new(file: impl Into<PathBuf>, checked: impl Into<String>) -> Patch {
        Patch {
            file: file.into(),
            checked: checked.into(),
            regions: Vec::new(),
        }
    }

    /// Build a patch by splicing each answer over its hole.
    ///
    /// `answers` pairs every hole with the code to put in it, or `None` to leave
    /// the hole unelaborated (a pinned hole, or one the model could not fill).
    ///
    /// This is the constructor callers should use. The checked text and the
    /// regions are produced by the *same* splice, so they cannot disagree: an
    /// answer of a different length than the `todo!(...)` it replaced shifts
    /// every later hole, and regions computed from some other splice would
    /// attribute errors to the wrong one.
    ///
    /// Fails if the holes' spans are stale or overlap, since splicing them would
    /// corrupt the file.
    pub fn splice(
        file: impl Into<PathBuf>,
        src: &str,
        answers: &[(&Hole, Option<&str>)],
    ) -> std::result::Result<Patch, String> {
        // Only filled holes become edits; the others keep their `todo!()`, which
        // compiles, so they contribute no region and need no check.
        let filled: Vec<usize> = answers
            .iter()
            .enumerate()
            .filter(|(_, (_, code))| code.is_some())
            .map(|(i, _)| i)
            .collect();

        // `splice_many` validates byte ranges but not that a hole still holds a
        // `todo!`; that check is the prober's and belongs with patching.
        for &i in &filled {
            let (hole, _) = answers[i];
            let slice = src
                .get(hole.byte_start..hole.byte_end)
                .ok_or_else(|| format!("hole {} is out of range", hole.location()))?;
            if !slice.starts_with(hole.macro_name.as_str()) {
                return Err(format!(
                    "{} no longer starts a `{}!` (found {:?}); the file changed since the holes \
                     were listed",
                    hole.location(),
                    hole.macro_name,
                    crate::util::truncate(slice, 40)
                ));
            }
        }

        let edits: Vec<Edit<'_>> = filled
            .iter()
            .map(|&i| {
                let (hole, code) = answers[i];
                Edit {
                    byte_start: hole.byte_start,
                    byte_end: hole.byte_end,
                    replacement: code.expect("filtered to filled holes"),
                }
            })
            .collect();

        let (checked, anchors) = splice_many(src, &edits)?;
        let regions = filled
            .iter()
            .zip(anchors)
            .map(|(&i, byte_start)| {
                let (hole, code) = answers[i];
                let code = code.expect("filtered to filled holes");
                Region {
                    hole: hole.clone(),
                    byte_start,
                    byte_end: byte_start + code.len(),
                }
            })
            .collect();

        Ok(Patch {
            file: file.into(),
            checked,
            regions,
        })
    }

    /// Attach the regions generated answers occupy.
    pub fn with_regions(mut self, regions: Vec<Region>) -> Patch {
        self.regions = regions;
        self
    }

    /// The region containing `byte`, if any.
    fn region_at(&self, byte: usize) -> Option<&Region> {
        // Regions are disjoint, so the first hit is the only hit.
        self.regions
            .iter()
            .find(|r| r.contains(byte))
            // A diagnostic can point just past the end of the code it is about
            // (rustc often spans the following token too), so fall back to the
            // nearest region that starts before the byte and is close by.
            .or_else(|| {
                self.regions
                    .iter()
                    .filter(|r| r.byte_start <= byte)
                    .min_by_key(|r| byte - r.byte_end.min(byte))
                    .filter(|r| byte - r.byte_end < NEAR_MISS)
            })
    }
}

/// How far past the end of generated code a diagnostic may sit and still be
/// blamed on it.
///
/// Small on purpose. This is a fallback for rustc spanning a trailing token, not
/// a blame window: a genuine error in generated code has a primary span inside
/// it, and a wide window here would let one answer swallow the next hole's error.
const NEAR_MISS: usize = 2;

/// One error the gate blames on a generated answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The hole whose generated code the error lands in, when one could be
    /// identified.
    ///
    /// `None` means the error sits outside every generated region, so it belongs
    /// to the crate as it already was rather than to anything this run produced.
    /// Reporting those separately matters: they are not the model's fault, and
    /// asking it to try again would never fix them.
    pub hole: Option<Hole>,
    /// The file the error was reported in.
    pub file: PathBuf,
    /// The error, one line, for reporting.
    pub message: String,
    pub code: Option<String>,
}

impl Failure {
    /// One line naming the hole, or the file when no hole owns the error.
    pub fn label(&self) -> String {
        match &self.hole {
            Some(hole) => format!("{}: {}", hole.location(), self.message),
            None => format!(
                "{}: {}",
                crate::util::display_rel_path(&self.file, &self.file),
                self.message
            ),
        }
    }
}

/// What the gate concluded about a generated tree.
#[derive(Debug)]
pub enum Verdict {
    /// The whole generated tree compiled. Every answer may be recorded.
    Accepted,
    /// It did not compile, so nothing may be recorded.
    Rejected {
        /// Every error, in the order rustc reported them. Some may have no hole:
        /// see [`Failure::hole`].
        failures: Vec<Failure>,
    },
    /// The gate could not run.
    ///
    /// Deliberately distinct from [`Verdict::Rejected`]: a missing cargo, a
    /// timeout, or a broken manifest says nothing about whether the generated
    /// code is correct. Reporting it as a rejection would blame the model for the
    /// environment.
    Inconclusive { reason: String },
}

impl Verdict {
    pub fn is_accepted(&self) -> bool {
        matches!(self, Verdict::Accepted)
    }

    pub fn is_rejected(&self) -> bool {
        matches!(self, Verdict::Rejected { .. })
    }

    /// The failures that belong to a generated answer, in report order.
    ///
    /// These are the ones worth re-asking the model about.
    pub fn attributable(&self) -> Vec<&Failure> {
        match self {
            Verdict::Rejected { failures } => {
                failures.iter().filter(|f| f.hole.is_some()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// The failures that belong to the crate as it already was.
    ///
    /// Retrying cannot fix these, so the caller must report them instead.
    pub fn pre_existing(&self) -> Vec<&Failure> {
        match self {
            Verdict::Rejected { failures } => {
                failures.iter().filter(|f| f.hole.is_none()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// One line for the run summary.
    pub fn label(&self) -> String {
        match self {
            Verdict::Accepted => "verified".to_string(),
            Verdict::Rejected { failures } => {
                let ours = self.attributable().len();
                let theirs = failures.len() - ours;
                match (ours, theirs) {
                    (0, _) => format!(
                        "not verified: {theirs} error(s), none in generated code -- the crate did \
                         not compile before this run either"
                    ),
                    (n, 0) => format!("not verified: {n} generated answer(s) did not compile"),
                    (n, t) => format!(
                        "not verified: {n} generated answer(s) did not compile, plus {t} error(s) \
                         elsewhere in the crate"
                    ),
                }
            }
            Verdict::Inconclusive { reason } => format!("not verified: {reason}"),
        }
    }
}

/// The compile gate.
///
/// Holds a [`Prober`] rather than its options so the check it runs is exactly the
/// one probing runs: same cargo binary, same timeout, same `--offline`, target
/// directory and `CARGO_HOME`.
pub struct Verifier {
    root: PathBuf,
    prober: Prober,
}

impl Verifier {
    pub fn new(root: impl Into<PathBuf>, options: ProberOptions) -> Verifier {
        let root = root.into();
        Verifier {
            prober: Prober::new(root.clone(), options),
            root,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Check the whole generated tree at once, and say which answers are at
    /// fault.
    ///
    /// Every patch is applied, one `cargo check` runs, and every file is restored
    /// byte-for-byte before this returns -- on every path, including a failed
    /// check and a timeout, because restoration is owned by [`RestoreGuard`]'s
    /// destructor.
    pub fn verify(&self, patches: &[Patch]) -> Verdict {
        if patches.is_empty() {
            // Nothing generated, so there is nothing to gate. Accepting is the
            // honest answer: the run produced no code, and the ledger gains
            // nothing from this call either way.
            return Verdict::Accepted;
        }

        // One lock for the whole gate, so no concurrent probe can restore a file
        // out from under the check. Released when this function returns.
        let _lock = match ProbeLock::acquire(&self.root, self.prober.lock_wait()) {
            Ok(lock) => lock,
            Err(e) => {
                return Verdict::Inconclusive {
                    reason: format!("cannot take the probe lock: {e:#}"),
                };
            }
        };

        // Every file is patched before the check runs. One at a time would leave
        // the others holding `todo!()`, which coerces to anything.
        let mut guards = Vec::with_capacity(patches.len());
        for patch in patches {
            match self.patch_one(patch) {
                Ok(guard) => guards.push(guard),
                Err(e) => {
                    // Restore whatever was already patched before reporting. The
                    // guards would do it on drop, but doing it here makes the
                    // ordering explicit rather than dependent on scope exit --
                    // and a restore failure is reported too, since a file left
                    // patched is worse than the original error.
                    let restore = restore_all(&mut guards);
                    let reason = match restore {
                        Ok(()) => format!("{e:#}"),
                        Err(r) => format!("{e:#}; and restoring the patched files failed: {r:#}"),
                    };
                    return Verdict::Inconclusive { reason };
                }
            }
        }

        // One check for the whole tree. This is the gate.
        let run = self.prober.check();
        // Put every file back before interpreting, so the tree is normal for the
        // whole duration of the (pure) decision logic and any early return.
        let restore_error = restore_all(&mut guards);

        let run = match run {
            Ok(run) => run,
            Err(e) => {
                return Verdict::Inconclusive {
                    reason: format!("{e:#}"),
                };
            }
        };
        if let Err(e) = restore_error {
            // A check that cannot be undone safely is not a check we can trust.
            return Verdict::Inconclusive {
                reason: format!("{e:#}"),
            };
        }

        if run.is_clean() {
            return Verdict::Accepted;
        }

        Verdict::Rejected {
            failures: run.errors().map(|d| self.classify(d, patches)).collect(),
        }
    }

    /// Write one file's generated text, keeping its original for restoration.
    fn patch_one(&self, patch: &Patch) -> Result<RestoreGuard> {
        let original = std::fs::read(&patch.file)
            .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", patch.file.display()))?;
        let src = String::from_utf8(original.clone())
            .map_err(|_| anyhow::anyhow!("{} is not valid UTF-8", patch.file.display()))?;

        // Whole-file replacement: the artifact is already the finished text, so
        // there is nothing to splice selectively. The span is validated by
        // `splice_many` on the way through.
        let edits = [Edit {
            byte_start: 0,
            byte_end: src.len(),
            replacement: &patch.checked,
        }];
        let (checked, _) = splice_many(&src, &edits)
            .map_err(|e| anyhow::anyhow!("cannot assemble {}: {e}", patch.file.display()))?;

        RestoreGuard::new(&patch.file, original, &checked)
    }

    /// Map one error onto the answer responsible for it.
    ///
    /// Attribution is positional: an error belongs to the hole whose generated
    /// code its primary span lands in. That is exact rather than heuristic,
    /// because the spans were computed by the same splice that produced the text
    /// rustc read.
    fn classify(&self, diagnostic: &Diagnostic, patches: &[Patch]) -> Failure {
        let primary = diagnostic.spans.iter().find(|s| s.is_primary);

        // Which checked file is the error in? Without a span in one of our files
        // it is about something else entirely -- a dependency, or the manifest.
        let located = primary.and_then(|span| {
            patches
                .iter()
                .find(|p| same_file(&self.root, &span.file_name, &p.file))
                .map(|p| (p, span.byte_start))
        });

        let (file, hole) = match located {
            Some((patch, byte)) => (
                patch.file.clone(),
                patch.region_at(byte).map(|r| r.hole.clone()),
            ),
            None => {
                // Report it against whatever file rustc named, so the message is
                // still actionable.
                let file = primary
                    .map(|s| {
                        let reported = Path::new(&s.file_name);
                        if reported.is_absolute() {
                            reported.to_path_buf()
                        } else {
                            self.root.join(reported)
                        }
                    })
                    .unwrap_or_else(|| self.root.clone());
                (file, None)
            }
        };

        Failure {
            hole,
            file,
            message: describe(diagnostic),
            code: diagnostic.code.clone(),
        }
    }
}

/// Restore every guard, reporting the first failure but still attempting the
/// rest: a file that cannot be restored must not stop the others from being.
fn restore_all(guards: &mut [RestoreGuard]) -> Result<()> {
    let mut first: Option<anyhow::Error> = None;
    for guard in guards.iter_mut() {
        if let Err(e) = guard.restore()
            && first.is_none()
        {
            first = Some(e);
        }
    }
    match first {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// One line for an error: the code, the headline, and the primary span label
/// when it says something the headline does not.
fn describe(diagnostic: &Diagnostic) -> String {
    let code = diagnostic
        .code
        .as_deref()
        .map(|c| format!("{c}: "))
        .unwrap_or_default();
    let label = diagnostic
        .spans
        .iter()
        .find(|s| s.is_primary)
        .and_then(|s| s.label.as_deref())
        .filter(|l| !l.is_empty());
    match label {
        Some(l) => format!("{code}{}: {l}", diagnostic.message),
        None => format!("{code}{}", diagnostic.message),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A throwaway crate with one source file.
    struct TempCrate {
        dir: PathBuf,
    }

    impl TempCrate {
        fn new(tag: &str, lib_rs: &str) -> TempCrate {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "cargo-hole-verify-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(
                dir.join("Cargo.toml"),
                "[package]\nname = \"verifycrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
                 \n[workspace]\n",
            )
            .unwrap();
            std::fs::write(dir.join("src/lib.rs"), lib_rs).unwrap();
            TempCrate { dir }
        }

        fn lib(&self) -> PathBuf {
            self.dir.join("src/lib.rs")
        }

        fn options(&self) -> ProberOptions {
            let cargo_home = std::env::temp_dir().join("cargo-hole-cargo-home");
            let _ = std::fs::create_dir_all(&cargo_home);
            ProberOptions::for_tests(self.dir.join("target"), cargo_home)
        }

        fn verifier(&self) -> Verifier {
            Verifier::new(self.dir.clone(), self.options())
        }

        fn holes(&self) -> Vec<Hole> {
            Hole::list_holes_one_file(&self.lib()).expect("extract holes")
        }
    }

    /// A patch holding the given answers, built by the same splice `fill` uses.
    ///
    /// `None` leaves a hole unelaborated, which is what a pinned or unfilled
    /// hole looks like in an artifact.
    fn patch(tc: &TempCrate, answers: &[Option<&str>]) -> Patch {
        let src = std::fs::read_to_string(tc.lib()).unwrap();
        let holes = tc.holes();
        assert_eq!(holes.len(), answers.len(), "one answer per hole");
        let pairs: Vec<(&Hole, Option<&str>)> =
            holes.iter().zip(answers).map(|(h, a)| (h, *a)).collect();
        Patch::splice(tc.lib(), &src, &pairs).expect("splice the answers")
    }

    // -- the trap ----------------------------------------------------------

    #[test]
    fn a_naive_root_check_would_accept_a_nonsense_artifact() {
        // This is the test that justifies the module. The real source is left
        // full of `todo!()`, which has type `!` and coerces to anything, so the
        // crate root compiles no matter what the model produced. A gate that
        // just ran `cargo check` on the root would report success here.
        let tc = TempCrate::new(
            "naive",
            "pub fn f() -> i64 {\n    todo!(\"spec: return the answer\")\n}\n",
        );

        // The crate as it stands (holes unelaborated) compiles: that is the trap.
        let naive = tc.verifier().verify(&[]);
        assert!(naive.is_accepted(), "an unfilled tree compiles by design");

        // But the artifact is nonsense, and the gate must catch it.
        let verdict = tc
            .verifier()
            .verify(&[patch(&tc, &[Some("\"definitely not an i64\"")])]);

        assert!(
            verdict.is_rejected(),
            "a gate that patches the artifact in must reject this: {verdict:?}"
        );
        assert_eq!(
            verdict.attributable().len(),
            1,
            "the error must be blamed on the hole: {verdict:?}"
        );
        assert_eq!(
            verdict.attributable()[0].hole.as_ref().unwrap().spec,
            "return the answer"
        );
    }

    #[test]
    fn the_real_source_is_untouched_by_a_rejection() {
        // The gate patches the real files, so a rejection must leave them exactly
        // as they were -- otherwise a failed run would destroy the user's source.
        let tc = TempCrate::new(
            "restore-reject",
            "pub fn f() -> i64 {\n    todo!(\"spec: answer\")\n}\n",
        );
        let before = std::fs::read(tc.lib()).unwrap();

        let verdict = tc.verifier().verify(&[Patch::new(
            tc.lib(),
            "pub fn f() -> i64 {\n    \"nope\"\n}\n",
        )]);

        assert!(verdict.is_rejected(), "{verdict:?}");
        assert_eq!(
            std::fs::read(tc.lib()).unwrap(),
            before,
            "the source must be restored byte-for-byte"
        );
        assert!(
            !tc.dir.join("src/lib.rs.cargo-hole.bak").exists(),
            "no backup may be left behind"
        );
    }

    #[test]
    fn a_good_artifact_is_accepted_and_the_source_is_restored() {
        let tc = TempCrate::new(
            "accept",
            "pub fn f() -> i64 {\n    todo!(\"spec: answer\")\n}\n",
        );
        let before = std::fs::read(tc.lib()).unwrap();

        let verdict = tc.verifier().verify(&[patch(&tc, &[Some("42")])]);

        assert!(verdict.is_accepted(), "{verdict:?}");
        assert_eq!(std::fs::read(tc.lib()).unwrap(), before);
        assert!(!tc.dir.join("src/lib.rs.cargo-hole.bak").exists());
    }

    #[test]
    fn several_answers_are_checked_in_one_gate() {
        let tc = TempCrate::new(
            "multi",
            "pub fn a() -> i64 {\n    todo!(\"spec: a\")\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: b\")\n}\n\n\
             pub fn c() -> String {\n    todo!(\"spec: c\")\n}\n",
        );
        let good = patch(&tc, &[Some("1"), Some("true"), Some("String::new()")]);
        let verdict = tc.verifier().verify(&[good]);
        assert!(verdict.is_accepted(), "{verdict:?}");

        // One bad answer in the middle: the rejection must name that hole only.
        let mixed = patch(&tc, &[Some("1"), Some("1 + 1"), Some("String::new()")]);
        let verdict = tc.verifier().verify(&[mixed]);
        assert!(verdict.is_rejected(), "{verdict:?}");
        let blamed = verdict.attributable();
        assert_eq!(blamed.len(), 1, "{verdict:?}");
        assert_eq!(blamed[0].hole.as_ref().unwrap().spec, "b");
    }

    #[test]
    fn a_cross_file_dependency_is_checked_with_every_file_patched() {
        // Why all files must be patched before the single check. `b.rs` calls
        // into `a.rs`; if `a.rs` were left as `todo!()` while `b.rs` was checked,
        // `!` would coerce to whatever `b.rs` needed and the pair would pass for
        // the wrong reason.
        let tc = TempCrate::new("crossfile", "pub mod a;\npub mod b;\n");
        std::fs::write(
            tc.dir.join("src/a.rs"),
            "pub fn value() -> i64 {\n    todo!(\"spec: value\")\n}\n",
        )
        .unwrap();
        std::fs::write(
            tc.dir.join("src/b.rs"),
            "pub fn doubled() -> i64 {\n    todo!(\"spec: doubled\")\n}\n",
        )
        .unwrap();

        // `a` returns the wrong type; `b` is fine on its own.
        let a_bad = "pub fn value() -> i64 {\n    \"not an i64\"\n}\n";
        let b_good = "pub fn doubled() -> i64 {\n    21\n}\n";

        let verdict = tc.verifier().verify(&[
            Patch::new(tc.dir.join("src/a.rs"), a_bad),
            Patch::new(tc.dir.join("src/b.rs"), b_good),
        ]);

        assert!(
            verdict.is_rejected(),
            "a bad answer in one file must fail the gate: {verdict:?}"
        );
    }

    #[test]
    fn an_empty_patch_list_is_accepted() {
        // Nothing was generated, so there is nothing to gate.
        let tc = TempCrate::new("empty", "pub fn f() -> i64 {\n    todo!(\"spec: x\")\n}\n");
        assert!(tc.verifier().verify(&[]).is_accepted());
    }

    #[test]
    fn an_unreadable_file_is_inconclusive_not_a_rejection() {
        // A missing file says nothing about whether the generated code is right,
        // so it must not be reported as a problem with the model's answer.
        let tc = TempCrate::new(
            "missing",
            "pub fn f() -> i64 {\n    todo!(\"spec: x\")\n}\n",
        );
        let verdict = tc
            .verifier()
            .verify(&[Patch::new(tc.dir.join("src/nope.rs"), "pub fn g() {}\n")]);
        assert!(
            matches!(verdict, Verdict::Inconclusive { .. }),
            "{verdict:?}"
        );
    }

    #[test]
    fn an_unparseable_artifact_is_rejected() {
        let tc = TempCrate::new("syntax", "pub fn f() -> i64 {\n    todo!(\"spec: x\")\n}\n");
        let verdict = tc
            .verifier()
            .verify(&[Patch::new(tc.lib(), "pub fn f() -> i64 { !!! }\n")]);
        assert!(verdict.is_rejected(), "{verdict:?}");
    }

    // -- pre-existing errors ----------------------------------------------

    #[test]
    fn an_error_outside_generated_code_is_not_blamed_on_a_hole() {
        // The crate is already broken at `broken`, which no answer touched. That
        // error must be reported without a hole, so the caller reports it rather
        // than asking the model to try again forever.
        let tc = TempCrate::new(
            "preexisting",
            "pub fn broken() -> i64 {\n    \"already wrong\"\n}\n\n\
             pub fn f() -> i64 {\n    todo!(\"spec: answer\")\n}\n",
        );
        let verdict = tc.verifier().verify(&[patch(&tc, &[Some("42")])]);

        assert!(verdict.is_rejected(), "{verdict:?}");
        assert!(
            verdict.attributable().is_empty(),
            "the answer was correct, so nothing may be blamed on it: {verdict:?}"
        );
        assert_eq!(verdict.pre_existing().len(), 1, "{verdict:?}");
        assert!(
            verdict.label().contains("before this run"),
            "{}",
            verdict.label()
        );
    }

    // -- attribution -------------------------------------------------------

    #[test]
    fn a_region_claims_a_diagnostic_inside_it() {
        let tc = TempCrate::new("region", "pub fn f() -> i64 {\n    todo!(\"spec: x\")\n}\n");
        let patch = patch(&tc, &[Some("1 + 1")]);
        let r = &patch.regions[0];
        assert_eq!(r.byte_end - r.byte_start, "1 + 1".len());
        assert_eq!(patch.region_at(r.byte_start).unwrap().hole.spec, "x");
        assert_eq!(patch.region_at(r.byte_end).unwrap().hole.spec, "x");
    }

    #[test]
    fn a_region_claims_a_span_that_runs_just_past_it() {
        // rustc often spans the following token as well as the offending
        // expression, so a byte right after the region must still resolve.
        let tc = TempCrate::new("past", "pub fn f() -> i64 {\n    todo!(\"spec: x\")\n}\n");
        let patch = patch(&tc, &[Some("1 + 1")]);
        let end = patch.regions[0].byte_end;
        assert!(patch.region_at(end + 1).is_some(), "one past is still ours");
        assert!(
            patch.region_at(end + NEAR_MISS + 1).is_none(),
            "but the window must stay tight"
        );
    }

    #[test]
    fn attribution_picks_the_right_hole_among_several() {
        let tc = TempCrate::new(
            "pick",
            "pub fn a() -> i64 {\n    todo!(\"spec: a\")\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: b\")\n}\n",
        );
        let patch = patch(&tc, &[Some("1 + 1"), Some("2 + 2")]);
        assert_eq!(
            patch
                .region_at(patch.regions[1].byte_start)
                .unwrap()
                .hole
                .spec,
            "b"
        );
        assert_eq!(
            patch
                .region_at(patch.regions[0].byte_start)
                .unwrap()
                .hole
                .spec,
            "a"
        );
    }

    #[test]
    fn a_long_answer_does_not_shift_blame_onto_the_next_hole() {
        // The bug `Patch::splice` exists to prevent. Answer `a` is much longer
        // than the `todo!(...)` it replaced, so hole `b` moves right. If the
        // regions were computed from anything but the splice that produced the
        // text, the error in `b` would be blamed on `a`.
        let tc = TempCrate::new(
            "shift",
            "pub fn a() -> i64 {\n    todo!(\"spec: a\")\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: b\")\n}\n",
        );
        // A long but correct answer for `a`, and a wrong one for `b`.
        let long_ok = "1i64 + 2i64 + 3i64 + 4i64 + 5i64";
        let patch = patch(&tc, &[Some(long_ok), Some("1 + 1")]);

        let verdict = tc.verifier().verify(&[patch]);
        assert!(verdict.is_rejected(), "{verdict:?}");
        let blamed = verdict.attributable();
        assert_eq!(
            blamed.len(),
            1,
            "exactly one answer is at fault: {verdict:?}"
        );
        assert_eq!(
            blamed[0].hole.as_ref().unwrap().spec,
            "b",
            "the shifted hole must still get the blame"
        );
    }

    #[test]
    fn an_unfilled_hole_contributes_no_region() {
        // A pinned or unfillable hole keeps its `todo!()`, which compiles. It
        // must not become a region, or an error near it would be blamed on an
        // answer that was never generated.
        let tc = TempCrate::new(
            "unfilled",
            "pub fn a() -> i64 {\n    todo!(\"spec: a\")\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: b\")\n}\n",
        );
        let patch = patch(&tc, &[None, Some("1 + 1")]);
        assert_eq!(patch.regions.len(), 1, "only the filled hole is a region");
        assert_eq!(patch.regions[0].hole.spec, "b");
        assert!(
            patch.checked.contains("todo!(\"spec: a\")"),
            "a stays unelaborated"
        );

        let verdict = tc.verifier().verify(&[patch]);
        assert!(verdict.is_rejected(), "{verdict:?}");
        assert_eq!(verdict.attributable()[0].hole.as_ref().unwrap().spec, "b");
    }

    #[test]
    fn splice_refuses_a_stale_span() {
        let tc = TempCrate::new(
            "stale-v",
            "pub fn f() -> i64 {\n    todo!(\"spec: x\")\n}\n",
        );
        let src = std::fs::read_to_string(tc.lib()).unwrap();
        let shifted = format!("// shifted\n{src}");
        let holes = tc.holes();
        let pairs: Vec<(&Hole, Option<&str>)> = holes.iter().map(|h| (h, Some("1"))).collect();
        let err = Patch::splice(tc.lib(), &shifted, &pairs).unwrap_err();
        assert!(err.contains("no longer starts"), "{err}");
    }

    #[test]
    fn describe_prefers_the_span_label() {
        let d = Diagnostic {
            code: Some("E0308".to_string()),
            level: "error".to_string(),
            message: "mismatched types".to_string(),
            spans: vec![crate::prober::Span {
                file_name: "src/lib.rs".to_string(),
                byte_start: 0,
                byte_end: 2,
                is_primary: true,
                label: Some("expected `i64`, found `&str`".to_string()),
            }],
        };
        let text = describe(&d);
        assert!(text.contains("E0308"), "{text}");
        assert!(text.contains("expected `i64`"), "{text}");
    }

    #[test]
    fn a_verdict_reports_what_can_and_cannot_be_retried() {
        let hole = Hole {
            file: PathBuf::from("/c/src/lib.rs"),
            byte_start: 0,
            byte_end: 5,
            spec: "x".to_string(),
            fn_sig: "fn f() -> i64".to_string(),
            fn_name: "f".to_string(),
            impl_ctx: None,
            line: 1,
            column: 1,
            macro_name: "todo".to_string(),
            position: crate::hole::HolePosition::Expression,
            pinned: false,
            unresolvable: None,
        };
        let mk = |hole: Option<Hole>| Failure {
            hole,
            file: PathBuf::from("/c/src/lib.rs"),
            message: "mismatched types".to_string(),
            code: Some("E0308".to_string()),
        };
        let verdict = Verdict::Rejected {
            failures: vec![mk(Some(hole.clone())), mk(None)],
        };
        assert_eq!(verdict.attributable().len(), 1);
        assert_eq!(verdict.pre_existing().len(), 1);
        assert!(
            verdict.label().contains("generated answer"),
            "{}",
            verdict.label()
        );
    }
}
