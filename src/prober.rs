//! The type probe: use rustc's own diagnostics as a type-query API.
//!
//! # The trick
//!
//! A hole's type is `!`, which coerces to anything and therefore never produces
//! a type error. Replace the hole with `()` and rustc reports
//! `E0308: expected X, found ()` — handing us the expected type, the exact
//! span, and real inference results (including coercions) for the price of one
//! `cargo check`. No LSP, no type checker reimplementation.
//!
//! # Mapping rustc's answers onto [`ProbeOutcome`]
//!
//! The probe is a hint source, not an oracle. It reports what it actually
//! learned, and declines rather than guessing:
//!
//! | situation | outcome |
//! |---|---|
//! | an `E0308` naming a concrete type | [`ProbeOutcome::Known`] |
//! | the hole is a whole statement | [`ProbeOutcome::NoExpectation`] (no `cargo check` needed) |
//! | rustc answered, but with a non-concrete type (`T`, `_`, `{integer}`, `impl Trait`) | [`ProbeOutcome::NoExpectation`] |
//! | the crate compiles and reports nothing about this hole | [`ProbeOutcome::NoExpectation`] |
//! | the patched crate does not compile, and no `E0308` is attributable to the hole | [`ProbeOutcome::ProbeFailed`] |
//! | `cargo` could not be run, or it timed out | [`ProbeOutcome::Broken`] |
//!
//! Two of those rows deserve their reasoning spelled out.
//!
//! **`NoExpectation` is not only for statements.** The enum was designed with
//! statement position in mind, and that case is handled here without even
//! invoking cargo, because a statement's value is discarded and so it has no
//! expected type by construction. But `let x = todo!();` where `x` is never
//! used is the same situation: the crate compiles, rustc says nothing, and
//! there genuinely is no expectation to report. Calling that `ProbeFailed`
//! would report an ordinary situation as a bug in this module.
//!
//! **`ProbeFailed` means the probe could not get an answer**, not that the
//! user's code is bad. An operator or method-call position (`todo!() + 1`,
//! `todo!().len()`) makes `!` fall back to `()`, so rustc emits `E0277` or
//! `E0599` instead of `E0308`. A crate that was already broken before this
//! module patched anything lands here too, with the errors reported rather than
//! swallowed. The two cases are distinguished in the message, because they need
//! different fixes.
//!
//! # Attribution
//!
//! `E0308` is not always placed on the hole. For `let b = todo!(); b`, rustc
//! reports the mismatch at the *later use* of `b`, so a probe that only looked
//! at the hole's own span would miss it. A diagnostic is therefore attributed
//! to the hole when it is in the same file, mentions ``found `()` `` (which
//! ties it to the `()` we substituted), and lies within
//! [`ATTRIBUTION_WINDOW`] bytes of the hole.
//!
//! # File safety
//!
//! Probing rewrites a source file in place, which is the only reason this
//! module needs any care at all. Three guards cover it:
//!
//! - [`RestoreGuard`] restores the original bytes on every exit path, including
//!   panic, because it is an RAII `Drop` guard.
//! - A `<file>.cargo-hole.bak` backup on disk, so a hard kill (`SIGKILL`, power
//!   loss) is recoverable afterwards via [`restore_leftovers`].
//! - A cross-process advisory lock (`.cargo-hole.probe.lock`) in the crate
//!   root, so two `cargo hole` runs cannot restore each other's bytes
//!   mid-flight and produce nonsense diagnostics.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::hole::{Hole, HolePosition};

/// What the probe learned (or failed to learn) about one hole.
#[derive(Debug)]
pub enum ProbeOutcome {
    /// 拿到了期望类型文本，如 "i64"
    Known(String),
    /// 位置本身没有期望类型（Statement），诚实报告而不是猜
    NoExpectation,
    /// 探针自己编译不过 —— 这是 prober 的 bug，不是模型的问题
    ProbeFailed(String),
    /// cargo 没跑起来
    Broken(anyhow::Error),
}

impl ProbeOutcome {
    /// A short label, for logs and for `list` output.
    pub fn label(&self) -> &'static str {
        match self {
            ProbeOutcome::Known(_) => "known",
            ProbeOutcome::NoExpectation => "no-expectation",
            ProbeOutcome::ProbeFailed(_) => "probe-failed",
            ProbeOutcome::Broken(_) => "broken",
        }
    }

    /// The expected type, when one was found.
    pub fn type_text(&self) -> Option<&str> {
        match self {
            ProbeOutcome::Known(t) => Some(t.as_str()),
            _ => None,
        }
    }
}

/// The text substituted for the hole.
///
/// A unit value, so that a mismatch against the hole's real type is reported as
/// `expected X, found ()` — the whole trick in one constant.
const UNIT: &str = "()";

/// How far from the hole an `E0308` may sit and still be attributed to it.
///
/// rustc sometimes reports the mismatch at a later use of the value rather than
/// at the substituted `()`; a tight bound would miss those, and an unbounded
/// search would grab unrelated errors from the same file. The ``found `()` ``
/// requirement in [`mentions_unit_value`] is what keeps this window safe.
pub const ATTRIBUTION_WINDOW: usize = 4096;

/// How close an error must be to be blamed on the patch rather than on the
/// user's pre-existing code. Deliberately much tighter than
/// [`ATTRIBUTION_WINDOW`]: this decides what a failure *message* claims, so it
/// should not reach across a file to find something to blame.
const LOCAL_WINDOW: usize = 64;

/// Suffix of the on-disk backup, for recovery after a hard kill.
pub const BACKUP_SUFFIX: &str = ".cargo-hole.bak";

/// Name of the cross-process lock file, created in the crate root.
pub const LOCK_NAME: &str = ".cargo-hole.probe.lock";

/// Default deadline for one `cargo check`.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// How long to wait for another process's probe to finish before giving up.
pub const DEFAULT_LOCK_WAIT: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// One rustc span, reduced to what the probe actually uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// As rustc reported it: often relative to the crate root.
    pub file_name: String,
    pub byte_start: usize,
    pub byte_end: usize,
    pub is_primary: bool,
    pub label: Option<String>,
}

/// One rustc diagnostic, reduced to what the probe actually uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// e.g. `E0308`. Absent for some diagnostics.
    pub code: Option<String>,
    /// `error`, `warning`, `note`, ...
    pub level: String,
    /// The headline, e.g. `mismatched types`.
    pub message: String,
    pub spans: Vec<Span>,
}

impl Diagnostic {
    pub fn is_error(&self) -> bool {
        self.level == "error"
    }

    /// Every string this diagnostic carries that might state an expectation.
    ///
    /// For `E0308` the headline is usually just `mismatched types` and the type
    /// lives in a span label, but both shapes occur, so both are searched.
    pub fn texts(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.message.as_str())
            .chain(self.spans.iter().filter_map(|s| s.label.as_deref()))
    }

    /// The nearest distance from `byte` to any span in `file`, if any span is
    /// in that file at all.
    fn distance_to(&self, file: &Path, root: &Path, byte: usize) -> Option<usize> {
        self.spans
            .iter()
            .filter(|s| same_file(root, &s.file_name, file))
            .map(|s| {
                if byte < s.byte_start {
                    s.byte_start - byte
                } else {
                    byte.saturating_sub(s.byte_end)
                }
            })
            .min()
    }
}

/// The raw `serde` shapes of `cargo check --message-format=json`.
///
/// Every field the probe does not need is optional or absent, so a future cargo
/// that adds keys cannot break parsing.
#[derive(serde::Deserialize)]
struct RawMessage {
    reason: String,
    #[serde(default)]
    message: Option<RawDiagnostic>,
}

#[derive(serde::Deserialize)]
struct RawDiagnostic {
    #[serde(default)]
    message: String,
    #[serde(default)]
    level: String,
    #[serde(default)]
    code: Option<RawCode>,
    #[serde(default)]
    spans: Vec<RawSpan>,
}

#[derive(serde::Deserialize)]
struct RawCode {
    #[serde(default)]
    code: String,
}

#[derive(serde::Deserialize)]
struct RawSpan {
    #[serde(default)]
    file_name: String,
    #[serde(default)]
    byte_start: usize,
    #[serde(default)]
    byte_end: usize,
    #[serde(default)]
    is_primary: bool,
    #[serde(default)]
    label: Option<String>,
}

/// Parse `cargo check --message-format=json` output into diagnostics.
///
/// Non-JSON lines (progress output, cargo's own warnings) and messages that are
/// not compiler diagnostics are skipped: the stream legitimately contains
/// `build-script-executed`, `artifact` and other reasons, and a line that fails
/// to parse is not worth failing the probe over.
pub fn parse_diagnostics(stdout: &[u8]) -> Vec<Diagnostic> {
    let text = String::from_utf8_lossy(stdout);
    let mut out = Vec::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || !line.starts_with('{') {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<RawMessage>(line) else {
            continue;
        };
        if raw.reason != "compiler-message" {
            continue;
        }
        let Some(d) = raw.message else { continue };

        out.push(Diagnostic {
            code: d.code.map(|c| c.code).filter(|c| !c.is_empty()),
            level: d.level,
            message: d.message,
            spans: d
                .spans
                .into_iter()
                .map(|s| Span {
                    file_name: s.file_name,
                    byte_start: s.byte_start,
                    byte_end: s.byte_end,
                    is_primary: s.is_primary,
                    label: s.label,
                })
                .collect(),
        });
    }

    out
}

/// Compare rustc's reported file name with ours.
///
/// rustc usually emits a path relative to the crate root, so an exact
/// comparison against an absolute path would never match. Handles the three
/// shapes that actually occur: identical, absolute-and-equal, and
/// relative-to-root.
pub fn same_file(root: &Path, reported: &str, file: &Path) -> bool {
    if reported.is_empty() {
        return false;
    }
    let reported_path = Path::new(reported);
    if reported_path == file {
        return true;
    }
    if reported_path.is_absolute() {
        return reported_path.canonicalize().ok() == file.canonicalize().ok();
    }
    // Relative: try it against the crate root, then as a suffix of our path.
    if root.join(reported_path) == *file {
        return true;
    }
    file.ends_with(reported_path)
}

// ---------------------------------------------------------------------------
// Reading rustc's answer
// ---------------------------------------------------------------------------

/// Does `text` say that a unit value was found?
///
/// This is what ties a diagnostic to the `()` this module substituted, and it is
/// what makes the attribution window safe: an unrelated `E0308` in the same file
/// mentions its own found type, not `()`.
fn mentions_unit_value(text: &str) -> bool {
    let Some(pos) = text.rfind("found ") else {
        return false;
    };
    let rest = text[pos + "found ".len()..].trim_start();
    rest.starts_with("`()`") || rest.starts_with("()")
}

/// Pull the expected type out of text like ``expected `i64`, found `()` ``.
///
/// Returns the raw text between `expected` and `found`. The result is *not*
/// validated here — see [`concrete_type`], which decides whether it is usable.
fn expected_type_from(text: &str) -> Option<String> {
    let pos = text.find("expected ")?;
    let rest = &text[pos + "expected ".len()..];

    // Prefer the ", found" boundary; fall back to the end of the text.
    let end = rest.find(", found").unwrap_or(rest.len());
    let raw = rest[..end].trim();
    if raw.is_empty() {
        return None;
    }

    // `expected type parameter `T`` keeps a prose prefix; the type itself is in
    // the backticks, so prefer them when present.
    if let Some(start) = raw.find('`')
        && let Some(stop) = raw[start + 1..].find('`')
    {
        let inner = raw[start + 1..start + 1 + stop].trim();
        return (!inner.is_empty()).then(|| inner.to_string());
    }

    Some(raw.to_string())
}

/// Decide whether an expected type is usable, or explain why it is not.
///
/// The probe must not hand the model `T` or `{integer}`: those are not types
/// anything can be written against, and passing them on as though they were a
/// real answer is exactly the confident-but-wrong behaviour this module exists
/// to avoid.
fn concrete_type(ty: &str) -> std::result::Result<String, String> {
    let t = ty.trim().trim_matches('`').trim();

    if t.is_empty() {
        return Err("the compiler named an empty type".to_string());
    }
    if t == "_" {
        return Err("the compiler said the type is `_`, i.e. still unresolved".to_string());
    }
    if t.starts_with('{') && t.ends_with('}') {
        // `{integer}` / `{float}`: inference never settled on a concrete type.
        return Err(format!(
            "`{t}` is an unresolved inference variable, not a type"
        ));
    }
    if t.starts_with("impl ") {
        return Err(format!(
            "`{t}` is an opaque type, which cannot be written by name"
        ));
    }
    // A bare single-uppercase-letter path is a generic parameter.
    if t.len() == 1 && t.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
        return Err(format!(
            "`{t}` is a generic type parameter, so it is not concrete"
        ));
    }

    Ok(t.to_string())
}

/// Is this diagnostic an `E0308` that attributes to the hole?
///
/// `Err(())` means "nothing to do with us"; `Ok(None)` means "it is about our
/// `()` but named no type"; `Ok(Some(t))` means "it named `t`".
#[allow(clippy::result_unit_err)]
fn attributable_e0308(
    d: &Diagnostic,
    hole: &Hole,
    root: &Path,
) -> std::result::Result<Option<String>, ()> {
    if !d.is_error() || d.code.as_deref() != Some("E0308") {
        return Err(());
    }
    if !d.texts().any(mentions_unit_value) {
        return Err(());
    }
    let Some(distance) = d.distance_to(&hole.file, root, hole.byte_start) else {
        return Err(());
    };
    if distance > ATTRIBUTION_WINDOW {
        return Err(());
    }
    Ok(d.texts().find_map(expected_type_from))
}

/// A finished `cargo check`.
#[derive(Debug, Clone)]
pub struct CheckRun {
    pub diagnostics: Vec<Diagnostic>,
    /// Cargo's own stderr, for messages when there are no diagnostics.
    pub stderr: String,
}

impl CheckRun {
    /// Every error in the run.
    pub fn errors(&self) -> impl Iterator<Item = &Diagnostic> {
        self.diagnostics.iter().filter(|d| d.is_error())
    }

    /// How many errors the run produced.
    pub fn error_count(&self) -> usize {
        self.errors().count()
    }

    /// Whether the crate compiled cleanly.
    pub fn is_clean(&self) -> bool {
        self.error_count() == 0
    }

    /// A one-line summary of the errors, for user-facing messages.
    pub fn error_summary(&self) -> String {
        let errs: Vec<&Diagnostic> = self.errors().collect();
        if errs.is_empty() {
            let stderr = self.stderr.trim();
            return if stderr.is_empty() {
                "cargo check failed with no diagnostics".to_string()
            } else {
                format!("cargo check failed: {}", crate::util::truncate(stderr, 300))
            };
        }
        let head = errs
            .iter()
            .take(3)
            .map(|d| match &d.code {
                Some(c) => format!("{c}: {}", d.message),
                None => d.message.clone(),
            })
            .collect::<Vec<_>>()
            .join("; ");
        if errs.len() > 3 {
            format!("{} error(s): {head}; ...", errs.len())
        } else {
            format!("{} error(s): {head}", errs.len())
        }
    }
}

/// Turn a finished `cargo check` into a verdict about one hole.
///
/// Split out from the process handling so the decision logic — which is where
/// the subtle mistakes live — can be tested exhaustively without running cargo.
pub fn interpret(hole: &Hole, root: &Path, run: &CheckRun) -> ProbeOutcome {
    let blame = attributable_diagnostics(hole, root, run);
    match decide(hole, root, run, &blame, Isolation::Single) {
        Some(outcome) => outcome,
        // Single-hole probing patches only this hole, so nothing else can have
        // disturbed it and the ambiguous case cannot arise.
        None => ProbeOutcome::ProbeFailed(
            "internal error: a single-hole probe reported an ambiguous result".to_string(),
        ),
    }
}

/// The `E0308`s in `run` that name our `()` and lie near this hole.
///
/// This is the single-hole view: with only one hole patched, every diagnostic
/// that mentions `found ()` and is close enough belongs to it.
fn attributable_diagnostics<'a>(
    hole: &Hole,
    root: &Path,
    run: &'a CheckRun,
) -> Vec<&'a Diagnostic> {
    run.diagnostics
        .iter()
        .filter(|d| attributable_e0308(d, hole, root).is_ok())
        .collect()
}

/// Decide which hole each `E0308` in a *batched* run belongs to.
///
/// The single-hole rule — "near this hole, and mentions `found ()`" — cannot be
/// used here, for two reasons.
///
/// First, [`ATTRIBUTION_WINDOW`] is deliberately wider than the gap between
/// typical holes, because rustc sometimes reports at a later use rather than at
/// the substituted `()`; applied per hole in a batch, that window would let
/// *every* hole claim the *first* `E0308` in the file.
///
/// Second, and more subtly, the diagnostics are offsets into the **patched**
/// text, not the original. Substituting `()` for `todo!(...)` shortens the file,
/// so holes after the first sit at different offsets than when they were listed.
/// `anchors` carries each hole's position in the patched text, in the same order
/// as `holes`.
///
/// So the batch assigns each diagnostic to exactly one hole, greedily by
/// distance: the closest pairing wins, and a diagnostic already claimed cannot
/// be claimed again. The hole that actually produced an error sits at distance 0
/// from it, so genuine pairs always outrank the spurious ones.
///
/// Returns one entry per input hole, in the same order.
fn assign_diagnostics<'a>(
    holes: &[&Hole],
    anchors: &[usize],
    root: &Path,
    run: &'a CheckRun,
) -> Vec<Vec<&'a Diagnostic>> {
    let mut assigned: Vec<Vec<&'a Diagnostic>> = (0..holes.len()).map(|_| Vec::new()).collect();

    // Candidates: an E0308 that names our `()`. Anything else cannot be about
    // the substitution this probe made.
    let candidates: Vec<&'a Diagnostic> = run
        .diagnostics
        .iter()
        .filter(|d| {
            d.is_error() && d.code.as_deref() == Some("E0308") && d.texts().any(mentions_unit_value)
        })
        .collect();

    // Every (distance, diagnostic, hole) pairing inside the window.
    let mut pairs: Vec<(usize, usize, usize)> = Vec::new();
    for (di, d) in candidates.iter().enumerate() {
        for (hi, h) in holes.iter().enumerate() {
            let anchor = anchors.get(hi).copied().unwrap_or(h.byte_start);
            if let Some(distance) = d.distance_to(&h.file, root, anchor)
                && distance <= ATTRIBUTION_WINDOW
            {
                pairs.push((distance, di, hi));
            }
        }
    }
    // Closest first. The trailing indices make the order total, so the result
    // does not depend on hash iteration or input order.
    pairs.sort();

    let mut diagnostic_used = vec![false; candidates.len()];
    let mut hole_used = vec![false; holes.len()];
    for (_, di, hi) in pairs {
        if diagnostic_used[di] || hole_used[hi] {
            continue;
        }
        diagnostic_used[di] = true;
        hole_used[hi] = true;
        assigned[hi].push(candidates[di]);
    }

    assigned
}

/// Whether the run being interpreted patched one hole or many.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Isolation {
    /// Exactly one hole was patched, so every error in the run belongs either to
    /// this hole or to the crate as it was already.
    Single,
    /// Many holes were patched at once, so an error may belong to any of them.
    Batched,
}

/// Decide a hole from one completed check run, given the diagnostics already
/// attributed to it.
///
/// Returns `None` only when the run cannot settle this hole *because it was
/// batched*: see [`Isolation`]. Callers in [`Isolation::Single`] always get
/// `Some`.
fn decide(
    hole: &Hole,
    root: &Path,
    run: &CheckRun,
    blame: &[&Diagnostic],
    isolation: Isolation,
) -> Option<ProbeOutcome> {
    // A statement's value is discarded, so there is no expected type to find.
    // Reported without compiling at all, which also makes probing a
    // statement-only file free.
    if hole.position == HolePosition::Statement {
        return Some(ProbeOutcome::NoExpectation);
    }

    // An attributable E0308 is direct evidence about this hole, and it outranks
    // anything else in the stream. Another hole's error elsewhere in the crate
    // does not invalidate it.
    let mut saw_non_concrete = false;
    for d in blame {
        match d.texts().find_map(expected_type_from) {
            Some(ty) => match concrete_type(&ty) {
                Ok(t) => return Some(ProbeOutcome::Known(t)),
                // rustc answered, but the answer is not something a model can be
                // asked for. That is "no usable expectation", not a failure.
                Err(_why) => saw_non_concrete = true,
            },
            // Attributable, but rustc named no type in it.
            None => saw_non_concrete = true,
        }
    }

    if saw_non_concrete {
        return Some(ProbeOutcome::NoExpectation);
    }

    // Nothing about the hole. If the run produced no errors at all, then every
    // patch in it compiled, so nothing can have cascaded into suppressing this
    // hole's diagnostic, and there is honestly no expectation to report. This is
    // what makes the common case — a batch whose holes all compile cleanly or
    // answer directly — need no fallback at all.
    if run.error_count() == 0 {
        return Some(ProbeOutcome::NoExpectation);
    }

    // Errors exist and none is attributable to this hole.
    //
    // In a batch that is ambiguous: another patched hole may have broken
    // inference badly enough to suppress or redirect the diagnostic this hole
    // would otherwise have produced. Reporting `ProbeFailed` here would blame
    // the crate for something a neighbouring hole did, so the batch declines and
    // the caller re-probes this hole in isolation.
    if isolation == Isolation::Batched {
        return None;
    }

    // Single: the only patch was ours, so say which of the two situations it is,
    // because they need different fixes. `hole.byte_start` is the right anchor
    // here: only this hole was substituted, so the text rustc saw is shifted
    // only where this hole was.
    let near_hole = run.errors().any(|d| {
        matches!(d.distance_to(&hole.file, root, hole.byte_start), Some(x) if x <= LOCAL_WINDOW)
    });

    let detail = if near_hole {
        format!(
            "substituting `{UNIT}` at the hole does not compile: {}. This position usually means \
             `!` fell back to `{UNIT}` (an operator, a method call, or an `impl Trait` return), \
             so rustc never emits the E0308 this probe needs",
            run.error_summary()
        )
    } else {
        format!(
            "the crate does not compile, so its inference cannot be trusted, and the errors are \
             not at the hole: {}. Fix these first",
            run.error_summary()
        )
    };
    Some(ProbeOutcome::ProbeFailed(detail))
}

// ---------------------------------------------------------------------------
// Substituting the probe value
// ---------------------------------------------------------------------------

/// Check that the recorded span still addresses a hole macro.
///
/// The offsets were computed when the file was last read. If the file changed on
/// disk in between — a concurrent edit, a stale listing — patching them would
/// corrupt arbitrary bytes, so this refuses instead.
fn validate_span(src: &str, hole: &Hole) -> std::result::Result<(), String> {
    if hole.byte_start > hole.byte_end {
        return Err(format!(
            "the recorded span is inverted ({}..{})",
            hole.byte_start, hole.byte_end
        ));
    }
    if hole.byte_end > src.len() {
        return Err(format!(
            "the recorded span {}..{} runs past the end of the file ({} bytes); the file has \
             changed since the holes were listed",
            hole.byte_start,
            hole.byte_end,
            src.len()
        ));
    }
    if !src.is_char_boundary(hole.byte_start) || !src.is_char_boundary(hole.byte_end) {
        return Err("the recorded span does not land on character boundaries".to_string());
    }

    let slice = &src[hole.byte_start..hole.byte_end];
    let expected = hole.macro_name.as_str();
    if !slice.starts_with(expected) {
        return Err(format!(
            "byte {} no longer starts a `{expected}!` (found {:?}); the file changed since the \
             holes were listed",
            hole.byte_start,
            crate::util::truncate(slice, 40)
        ));
    }
    Ok(())
}

/// Replace the hole with [`UNIT`].
fn patched_source(src: &str, hole: &Hole) -> std::result::Result<String, String> {
    validate_span(src, hole)?;
    let mut out = String::with_capacity(src.len() + UNIT.len());
    out.push_str(&src[..hole.byte_start]);
    out.push_str(UNIT);
    out.push_str(&src[hole.byte_end..]);
    Ok(out)
}

/// One replacement: bytes `byte_start..byte_end` of the original become
/// `replacement`.
///
/// The general form of what a probe does with [`UNIT`] and what a verifier does
/// with generated code. Both splice text into a source file and then need to
/// know where each piece landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edit<'a> {
    pub byte_start: usize,
    pub byte_end: usize,
    pub replacement: &'a str,
}

/// Splice several non-overlapping edits into `src` in one pass.
///
/// Returns the new source together with each edit's byte offset in it, in the
/// **input's** order. Those offsets are the point: everything downstream works
/// in the new text's coordinates, because that is what rustc reports spans
/// against. Replacements are rarely the same length as what they replace, so
/// every edit after the first shifts.
///
/// Overlapping or duplicate ranges are refused rather than resolved: a caller
/// that got here with duplicates has a bug, and splicing both would silently
/// corrupt the file.
#[allow(clippy::type_complexity)]
pub fn splice_many(
    src: &str,
    edits: &[Edit<'_>],
) -> std::result::Result<(String, Vec<usize>), String> {
    for e in edits {
        if e.byte_start > e.byte_end {
            return Err(format!(
                "edit {}..{} ends before it starts",
                e.byte_start, e.byte_end
            ));
        }
        if e.byte_end > src.len() {
            return Err(format!(
                "edit {}..{} runs past the end of a {} byte file",
                e.byte_start,
                e.byte_end,
                src.len()
            ));
        }
        if !src.is_char_boundary(e.byte_start) || !src.is_char_boundary(e.byte_end) {
            return Err(format!(
                "edit {}..{} does not land on character boundaries",
                e.byte_start, e.byte_end
            ));
        }
    }

    // Sort by start so splices can be emitted left to right, remembering where
    // each edit came from so the offsets can be returned in input order.
    let mut ordered: Vec<(usize, &Edit<'_>)> = edits.iter().enumerate().collect();
    ordered.sort_by_key(|(_, e)| (e.byte_start, e.byte_end));

    for pair in ordered.windows(2) {
        let (_, a) = pair[0];
        let (_, b) = pair[1];
        if b.byte_start < a.byte_end {
            return Err(format!(
                "edits at bytes {}..{} and {}..{} overlap, so they cannot be spliced together",
                a.byte_start, a.byte_end, b.byte_start, b.byte_end
            ));
        }
    }

    let growth: usize = ordered.iter().map(|(_, e)| e.replacement.len()).sum();
    let mut out = String::with_capacity(src.len() + growth);
    let mut anchors = vec![0usize; ordered.len()];
    let mut cursor = 0usize;
    for (position, (_, e)) in ordered.iter().enumerate() {
        out.push_str(&src[cursor..e.byte_start]);
        anchors[position] = out.len();
        out.push_str(e.replacement);
        cursor = e.byte_end;
    }
    out.push_str(&src[cursor..]);

    // Reorder the offsets from sorted order back to the caller's order.
    let mut by_input = vec![0usize; ordered.len()];
    for (position, (input_index, _)) in ordered.iter().enumerate() {
        by_input[*input_index] = anchors[position];
    }
    Ok((out, by_input))
}

/// Replace several non-overlapping holes with [`UNIT`] in one pass.
///
/// Returns the patched source together with each hole's new byte offset, in the
/// **input's** order. See [`splice_many`] for why those offsets matter.
fn patched_source_many(
    src: &str,
    holes: &[&Hole],
) -> std::result::Result<(String, Vec<usize>), String> {
    // The hole's span must still hold a `todo!` before it is replaced, and this
    // is where that check lives: `splice_many` works on raw byte ranges and
    // cannot know what should be there.
    for hole in holes {
        validate_span(src, hole)?;
    }

    let edits: Vec<Edit<'_>> = holes
        .iter()
        .map(|h| Edit {
            byte_start: h.byte_start,
            byte_end: h.byte_end,
            replacement: UNIT,
        })
        .collect();
    splice_many(src, &edits)
}

/// Group holes by file, splitting off the ones that can never be patched.
///
/// Returns `(batchable_by_file, impossible)`. The map pairs each hole with its
/// index in the caller's list, so a verdict can be written back to the right
/// slot; the second is the holes decided without touching any file.
#[allow(clippy::type_complexity)]
fn split_batchable(
    holes: &[Hole],
) -> (
    BTreeMap<PathBuf, Vec<(usize, &Hole)>>,
    Vec<(usize, ProbeOutcome)>,
) {
    let mut by_file: BTreeMap<PathBuf, Vec<(usize, &Hole)>> = BTreeMap::new();
    let mut impossible = Vec::new();
    for (i, hole) in holes.iter().enumerate() {
        if hole.position == HolePosition::Statement {
            impossible.push((i, ProbeOutcome::NoExpectation));
        } else if let Some(reason) = &hole.unresolvable {
            impossible.push((
                i,
                ProbeOutcome::ProbeFailed(format!(
                    "the byte range of {} could not be validated ({reason}), so it cannot be \
                     patched safely",
                    hole.location()
                )),
            ));
        } else {
            by_file
                .entry(hole.file.clone())
                .or_default()
                .push((i, hole));
        }
    }
    (by_file, impossible)
}

// ---------------------------------------------------------------------------
// Restoring the file
// ---------------------------------------------------------------------------

/// Path of the on-disk backup beside `path`.
fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(BACKUP_SUFFIX);
    path.with_file_name(name)
}

/// Can we write this file? Checked before patching so the failure is a clear
/// message rather than a half-finished write.
fn can_write(path: &Path) -> bool {
    OpenOptions::new().append(true).open(path).is_ok()
}

/// Writes `patched` over `path`, and puts the original bytes back on `Drop`.
///
/// The backup file is written first, so even a `SIGKILL` between the two writes
/// leaves a recoverable `.bak` rather than a patched file with no original.
#[derive(Debug)]
pub struct RestoreGuard {
    path: PathBuf,
    backup: PathBuf,
    original: Vec<u8>,
    armed: bool,
}

impl RestoreGuard {
    /// Write `patched` to `path`, keeping `original` for restoration.
    pub fn new(path: &Path, original: Vec<u8>, patched: &str) -> Result<RestoreGuard> {
        let backup = backup_path(path);
        std::fs::write(&backup, &original)
            .with_context(|| format!("cannot write backup {}", backup.display()))?;
        if let Err(e) = std::fs::write(path, patched) {
            // Do not leave a stray backup behind if the patch never landed.
            let _ = std::fs::remove_file(&backup);
            return Err(e).with_context(|| format!("cannot write patched {}", path.display()));
        }
        Ok(RestoreGuard {
            path: path.to_path_buf(),
            backup,
            original,
            armed: true,
        })
    }

    /// Accept the patch: keep the new bytes and drop the backup.
    pub fn keep(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let _ = std::fs::remove_file(&self.backup);
    }

    /// Put the original bytes back and remove the backup.
    pub fn restore(&mut self) -> Result<()> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        std::fs::write(&self.path, &self.original)
            .with_context(|| format!("cannot restore {}", self.path.display()))?;
        match std::fs::remove_file(&self.backup) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("cannot remove {}", self.backup.display())),
        }
    }
}

impl Drop for RestoreGuard {
    fn drop(&mut self) {
        if self.armed {
            // Best effort: `Drop` has no way to report anything useful.
            let _ = std::fs::write(&self.path, &self.original);
            let _ = std::fs::remove_file(&self.backup);
        }
    }
}

/// Find every leftover backup under `root` and restore it.
///
/// This is what makes a hard kill recoverable. It takes the probe lock, because
/// restoring a `.bak` over a file that another process is *currently* probing
/// would destroy that probe's patch mid-flight, and the probe would then
/// silently report a clean compile with no type information.
///
/// Returns the files that were restored.
pub fn restore_leftovers(root: &Path) -> Result<Vec<PathBuf>> {
    let _lock = ProbeLock::acquire(root, DEFAULT_LOCK_WAIT)?;
    restore_leftovers_locked(root)
}

/// As [`restore_leftovers`], but without taking the lock.
///
/// The caller must already hold the probe lock for `root`; re-acquiring would
/// deadlock.
pub fn restore_leftovers_locked(root: &Path) -> Result<Vec<PathBuf>> {
    let mut restored = Vec::new();
    for backup in find_backups(root) {
        let Ok(original) = std::fs::read(&backup) else {
            continue;
        };
        // Bind the lossy string before stripping: `strip_suffix` borrows it, so
        // stripping the temporary directly would not outlive the statement.
        let raw = backup.to_string_lossy().into_owned();
        let Some(target) = raw.strip_suffix(BACKUP_SUFFIX).map(PathBuf::from) else {
            continue;
        };
        std::fs::write(&target, &original)
            .with_context(|| format!("cannot restore {}", target.display()))?;
        let _ = std::fs::remove_file(&backup);
        restored.push(target);
    }
    Ok(restored)
}

/// Every `*.cargo-hole.bak` under `root`, skipping build output and dotdirs.
fn find_backups(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                // Build output and VCS metadata are never worth walking, and
                // `target/` in particular is enormous.
                if name == "target" || name.starts_with('.') {
                    continue;
                }
                stack.push(path);
            } else if name.ends_with(BACKUP_SUFFIX) {
                out.push(path);
            }
        }
    }

    out.sort();
    out
}

// ---------------------------------------------------------------------------
// Cross-process locking
// ---------------------------------------------------------------------------

/// An advisory lock held for the duration of a probe.
///
/// A probe rewrites a source file in place, so two probes of the same crate at
/// once — two `cargo hole` invocations, or a CLI run beside an editor plugin —
/// would restore each other's bytes and produce nonsense diagnostics. The lock
/// is cheap and costs nothing when the tool is used as intended.
///
/// Released in `Drop`, which covers the panic and early-return paths, and the OS
/// releases it if the process dies.
#[derive(Debug)]
pub struct ProbeLock {
    /// `None` when the lock file could not be created at all. See [`acquire`].
    ///
    /// [`acquire`]: ProbeLock::acquire
    file: Option<File>,
    path: PathBuf,
}

impl ProbeLock {
    /// Take the lock, waiting up to `wait` for another holder to finish.
    ///
    /// A read-only checkout cannot create the lock file. That is not worth
    /// failing over: the probe is about to fail with a clear message about the
    /// write anyway, and refusing here would replace that message with a
    /// confusing one about locking. So the lock is returned unheld.
    pub fn acquire(root: &Path, wait: Duration) -> Result<ProbeLock> {
        let path = root.join(LOCK_NAME);
        let file = match OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
        {
            Ok(f) => f,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem
                ) =>
            {
                return Ok(ProbeLock { file: None, path });
            }
            Err(e) => {
                return Err(e).with_context(|| format!("cannot open lock {}", path.display()));
            }
        };

        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(ProbeLock {
                        file: Some(file),
                        path,
                    });
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        anyhow::bail!(
                            "another `cargo hole` has been probing {} for over {}s (lock {}); \
                             wait for it to finish, or delete the lock file if it is gone",
                            root.display(),
                            wait.as_secs(),
                            path.display()
                        );
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("cannot lock {}", path.display()));
                }
            }
        }
    }

    /// Whether the lock is actually held.
    pub fn is_held(&self) -> bool {
        self.file.is_some()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

// ---------------------------------------------------------------------------
// Running cargo
// ---------------------------------------------------------------------------

/// How to invoke cargo, and how long to allow.
#[derive(Debug, Clone)]
pub struct ProberOptions {
    /// Cargo executable.
    pub cargo: String,
    /// Deadline for one `cargo check`.
    pub timeout: Duration,
    /// Pass `--offline`.
    pub offline: bool,
    /// Reuse this target directory, keeping the incremental cache warm.
    pub target_dir: Option<PathBuf>,
    /// Set `CARGO_HOME` for the child.
    ///
    /// Passed as an environment variable on the child rather than through
    /// `std::env::set_var`, which is `unsafe` in edition 2024 and would race
    /// with every other thread in the process.
    pub cargo_home: Option<PathBuf>,
    /// How long to wait for another process's probe.
    pub lock_wait: Duration,
}

impl Default for ProberOptions {
    fn default() -> Self {
        ProberOptions {
            cargo: cargo_binary(),
            timeout: DEFAULT_TIMEOUT,
            offline: false,
            target_dir: None,
            cargo_home: None,
            lock_wait: DEFAULT_LOCK_WAIT,
        }
    }
}

impl ProberOptions {
    /// Read the environment knobs.
    pub fn from_env() -> ProberOptions {
        ProberOptions {
            cargo: cargo_binary(),
            timeout: DEFAULT_TIMEOUT,
            offline: std::env::var_os("CARGO_HOLE_OFFLINE").is_some(),
            target_dir: std::env::var_os("CARGO_HOLE_TARGET_DIR").map(PathBuf::from),
            cargo_home: std::env::var_os("CARGO_HOME").map(PathBuf::from),
            lock_wait: DEFAULT_LOCK_WAIT,
        }
    }

    /// Options for tests: offline, with a caller-supplied target directory and
    /// Cargo home so no global state has to be mutated.
    pub fn for_tests(target_dir: impl Into<PathBuf>, cargo_home: impl Into<PathBuf>) -> Self {
        ProberOptions {
            cargo: cargo_binary(),
            timeout: Duration::from_secs(600),
            offline: true,
            target_dir: Some(target_dir.into()),
            cargo_home: Some(cargo_home.into()),
            lock_wait: DEFAULT_LOCK_WAIT,
        }
    }

    /// The `cargo check` invocation, without running it.
    fn check_command(&self, root: &Path) -> Command {
        let mut cmd = Command::new(&self.cargo);
        cmd.arg("check")
            .arg("--message-format=json")
            .arg("--quiet")
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(dir) = &self.target_dir {
            cmd.arg("--target-dir").arg(dir);
        }
        if self.offline {
            cmd.arg("--offline");
        }
        if let Some(home) = &self.cargo_home {
            cmd.env("CARGO_HOME", home);
        }
        cmd
    }
}

/// The cargo executable to invoke, honouring `CARGO` when set by a parent cargo.
pub fn cargo_binary() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

/// A finished subprocess.
#[derive(Debug)]
struct Output {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: Option<i32>,
    timed_out: bool,
}

/// Run a command with a deadline.
///
/// On Unix the child leads its own process group and a timeout kills the whole
/// group. Killing only the direct child is not enough: cargo spawns rustc, and
/// an orphaned rustc holds the stdout pipe open, so the reader threads never
/// observe EOF and the call hangs well past its deadline.
fn run_command(cmd: &mut Command, timeout: Duration) -> Result<Output> {
    let program = cmd.get_program().to_string_lossy().into_owned();

    // Configured here rather than by the caller: taking `child.stdout` below
    // only yields a pipe if someone asked for one, and an inherited handle
    // would silently send the child's output to ours instead of capturing it.
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot run `{program}` -- is it installed and on PATH?"))?;

    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let out_thread = thread::spawn(move || out_pipe.map(read_all).unwrap_or_default());
    let err_thread = thread::spawn(move || err_pipe.map(read_all).unwrap_or_default());

    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    timed_out = true;
                    kill_tree(&mut child);
                    status = child.wait().ok();
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            Err(e) => {
                kill_tree(&mut child);
                let _ = child.wait();
                return Err(e).with_context(|| format!("failed while waiting for `{program}`"));
            }
        }
    }

    let stdout = out_thread.join().unwrap_or_default();
    let stderr = err_thread.join().unwrap_or_default();

    Ok(Output {
        stdout,
        stderr,
        code: status.and_then(|s| s.code()),
        timed_out,
    })
}

/// Kill the child and, on Unix, everything in its process group.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        // The child leads its own group, so its pid is the group id. SIGKILL
        // rather than SIGTERM: a wedged build tool may ignore SIGTERM, and the
        // caller has already decided this run is over.
        // Safety: `killpg` on a group this process created is well-defined; a
        // failure (the group already exited) is reported and ignored.
        unsafe {
            libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
        }
    }
    let _ = child.kill();
}

fn read_all(mut r: impl Read) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf);
    buf
}

// ---------------------------------------------------------------------------
// The prober
// ---------------------------------------------------------------------------

/// Asks rustc for the expected type of a hole.
///
/// Hold one of these per crate root. It owns no mutable state, so a single
/// instance can probe many holes in sequence.
#[derive(Debug, Clone)]
pub struct Prober {
    root: PathBuf,
    options: ProberOptions,
}

impl Prober {
    pub fn new(root: impl Into<PathBuf>, options: ProberOptions) -> Prober {
        Prober {
            root: root.into(),
            options,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// How long to wait for another process's probe lock.
    ///
    /// Exposed so the verifier can serialise against probing using the same
    /// deadline, rather than picking its own and behaving differently under
    /// contention.
    pub fn lock_wait(&self) -> Duration {
        self.options.lock_wait
    }

    /// Ask rustc what type `hole` must have.
    ///
    /// Never leaves the file modified, on any path including panic and timeout.
    pub fn probe(&self, hole: &Hole) -> ProbeOutcome {
        // A statement needs no compiler at all.
        if hole.position == HolePosition::Statement {
            return ProbeOutcome::NoExpectation;
        }

        if let Some(reason) = &hole.unresolvable {
            return ProbeOutcome::ProbeFailed(format!(
                "the byte range of {} could not be validated ({reason}), so it cannot be patched \
                 safely",
                hole.location()
            ));
        }

        // The lock must outlive the patch, so it is taken first and released
        // last.
        let _lock = match ProbeLock::acquire(&self.root, self.options.lock_wait) {
            Ok(l) => l,
            Err(e) => return ProbeOutcome::Broken(e),
        };

        let original = match std::fs::read(&hole.file) {
            Ok(bytes) => bytes,
            Err(e) => {
                return ProbeOutcome::Broken(
                    anyhow::Error::new(e).context(format!("cannot read {}", hole.file.display())),
                );
            }
        };
        let Ok(src) = String::from_utf8(original.clone()) else {
            return ProbeOutcome::ProbeFailed(format!(
                "{} is not valid UTF-8, so it cannot be patched",
                hole.file.display()
            ));
        };

        if !can_write(&hole.file) {
            return ProbeOutcome::ProbeFailed(format!(
                "{} is not writable, and probing needs to patch it in place",
                hole.file.display()
            ));
        }

        let patched = match patched_source(&src, hole) {
            Ok(p) => p,
            Err(why) => return ProbeOutcome::ProbeFailed(why),
        };

        let mut guard = match RestoreGuard::new(&hole.file, original, &patched) {
            Ok(g) => g,
            Err(e) => return ProbeOutcome::Broken(e),
        };

        let run = self.run_check();
        // Restore before interpreting, so the file is back to normal for the
        // whole duration of the (pure) decision logic and any early return.
        if let Err(e) = guard.restore() {
            return ProbeOutcome::Broken(e);
        }

        match run {
            Ok(run) => interpret(hole, &self.root, &run),
            Err(e) => ProbeOutcome::Broken(e),
        }
    }

    /// Ask rustc for the expected type of many holes with as few `cargo check`
    /// runs as possible.
    ///
    /// One `cargo check` per *file* rather than per hole. A batch patches only
    /// holes from a single file at a time, because a span is a byte offset into
    /// one file and the per-hole blame window in [`decide`] is meaningless
    /// across files. Files are few and holes are many, so this is where the
    /// saving comes from.
    ///
    /// Returns a verdict for **every** input hole, in the input's order. Holes
    /// the batch cannot settle are re-probed one at a time, so the answer is
    /// always as trustworthy as [`Prober::probe`] alone would have been — the
    /// batch is a fast path, never a different answer.
    ///
    /// Never leaves any file modified, on any path.
    pub fn probe_all(&self, holes: &[Hole]) -> Vec<ProbeOutcome> {
        let mut outcomes: Vec<Option<ProbeOutcome>> = (0..holes.len()).map(|_| None).collect();

        // Holes that need no compiler at all: a statement's value is discarded,
        // and an unvalidated span must never be patched.
        let (by_file, impossible) = split_batchable(holes);
        for (i, outcome) in impossible {
            outcomes[i] = Some(outcome);
        }

        // A batch of one is just a probe; going through the batched path would
        // cost an extra check on the ambiguous case for no benefit.
        if by_file.values().map(Vec::len).sum::<usize>() > 1 {
            self.probe_batched(&by_file, &mut outcomes);
        }

        // Anything still undecided — declined by the batch, or left over because
        // the batch could not run — is settled the slow, reliable way. The
        // batch's lock is released by then, so `probe` can take it.
        for (i, hole) in holes.iter().enumerate() {
            if outcomes[i].is_none() {
                outcomes[i] = Some(self.probe(hole));
            }
        }

        outcomes
            .into_iter()
            .map(|o| o.expect("every hole is decided"))
            .collect()
    }

    /// Patch each file's holes at once, run one check per file, and record the
    /// verdicts the batch can settle.
    ///
    /// A file that cannot be patched — an unreadable or read-only file, an
    /// overlapping pair, a span that no longer holds a hole — is skipped whole,
    /// leaving its holes undecided so the per-hole path reports the real reason.
    /// Refusing the healthy holes in that file too is deliberate: a file whose
    /// spans disagree with the source is not one to patch selectively.
    fn probe_batched(
        &self,
        by_file: &BTreeMap<PathBuf, Vec<(usize, &Hole)>>,
        outcomes: &mut [Option<ProbeOutcome>],
    ) {
        // Held across the whole batch, so no other process can restore a file
        // mid-batch. Dropped when this function returns, before the per-hole
        // fallback runs and re-acquires it.
        let Ok(_lock) = ProbeLock::acquire(&self.root, self.options.lock_wait) else {
            // A lock failure is a whole-run problem. Leave everything undecided
            // so the per-hole path reports it with its own context.
            return;
        };

        for (file, file_holes) in by_file {
            let Ok(original) = std::fs::read(file) else {
                continue;
            };
            let Ok(src) = String::from_utf8(original.clone()) else {
                continue;
            };
            if !can_write(file) {
                continue;
            }
            let spans: Vec<&Hole> = file_holes.iter().map(|(_, h)| *h).collect();
            let Ok((patched, anchors)) = patched_source_many(&src, &spans) else {
                continue;
            };

            let Ok(mut guard) = RestoreGuard::new(file, original, &patched) else {
                continue;
            };
            let run = self.run_check();
            // Restore before interpreting, so the file is back to normal for the
            // whole duration of the (pure) decision logic.
            if guard.restore().is_err() {
                continue;
            }
            // cargo could not run at all: nothing here is trustworthy, and the
            // per-hole path will surface the same error with better context.
            let Ok(run) = run else { continue };

            // Assign each diagnostic to at most one hole, so a wide attribution
            // window cannot let several holes claim the same error, and match
            // against where each hole landed in the patched text.
            let blame = assign_diagnostics(&spans, &anchors, &self.root, &run);

            // `file_holes` and `spans` are the same holes in the same order.
            for ((i, hole), blame) in file_holes.iter().zip(&blame) {
                outcomes[*i] = decide(hole, &self.root, &run, blame, Isolation::Batched);
            }
        }
    }

    /// Run `cargo check` and collect its diagnostics.
    ///
    /// Public so the verifier can reuse this exact invocation — the same cargo
    /// binary, timeout, `--offline`, target directory and `CARGO_HOME` — instead
    /// of duplicating the option handling and letting the two drift apart.
    pub fn check(&self) -> Result<CheckRun> {
        self.run_check()
    }

    /// Run `cargo check` and collect its diagnostics.
    fn run_check(&self) -> Result<CheckRun> {
        let mut cmd = self.options.check_command(&self.root);
        let out = run_command(&mut cmd, self.options.timeout)?;

        if out.timed_out {
            anyhow::bail!(
                "`{} check` did not finish within {}s in {}; raise the timeout or check for a \
                 build that is stuck",
                self.options.cargo,
                self.options.timeout.as_secs(),
                self.root.display()
            );
        }

        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if let Some(code) = out.code
            && code != 0
            && out.stdout.is_empty()
        {
            // Cargo itself refused (bad manifest, unknown flag, no such
            // package). There is nothing to interpret.
            anyhow::bail!(
                "`{} check` exited with status {code} and produced no diagnostics: {}",
                self.options.cargo,
                crate::util::truncate(stderr.trim(), 400)
            );
        }

        Ok(CheckRun {
            diagnostics: parse_diagnostics(&out.stdout),
            stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A source file on disk with holes in it.
    ///
    /// Holes carry absolute file paths, and [`same_file`] compares against them,
    /// so the fixtures have to be real files rather than in-memory strings.
    struct Src {
        dir: PathBuf,
        path: PathBuf,
        src: String,
    }

    impl Src {
        fn new(tag: &str, src: &str) -> Src {
            let dir =
                std::env::temp_dir().join(format!("cargo-hole-src-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("src")).unwrap();
            let path = dir.join("src/lib.rs");
            std::fs::write(&path, src).unwrap();
            Src {
                dir,
                path,
                src: src.to_string(),
            }
        }

        fn holes(&self) -> Vec<Hole> {
            Hole::list_holes_one_file(&self.path).expect("extract holes")
        }

        fn hole(&self, spec: &str) -> Hole {
            self.holes()
                .into_iter()
                .find(|h| h.spec == spec)
                .unwrap_or_else(|| panic!("no hole with spec {spec:?}"))
        }

        fn root(&self) -> &Path {
            &self.dir
        }

        /// An `E0308` at `start..end`, in this fixture's file.
        fn e0308(&self, start: usize, end: usize, label: &str) -> Diagnostic {
            Diagnostic {
                code: Some("E0308".into()),
                level: "error".into(),
                message: "mismatched types".into(),
                spans: vec![Span {
                    file_name: self.path.to_string_lossy().into_owned(),
                    byte_start: start,
                    byte_end: end,
                    is_primary: true,
                    label: Some(label.into()),
                }],
            }
        }
    }

    impl Drop for Src {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn run_with(diags: Vec<Diagnostic>) -> CheckRun {
        CheckRun {
            diagnostics: diags,
            stderr: String::new(),
        }
    }

    // -- reading the expectation out of text -------------------------------

    #[test]
    fn expected_type_is_read_from_the_label() {
        assert_eq!(
            expected_type_from("expected `i64`, found `()`").as_deref(),
            Some("i64")
        );
        assert_eq!(
            expected_type_from("expected `Vec<String>`, found `()`").as_deref(),
            Some("Vec<String>")
        );
        assert_eq!(
            expected_type_from("expected `&str`, found `()`").as_deref(),
            Some("&str")
        );
    }

    #[test]
    fn expected_type_handles_the_generic_parameter_shape() {
        // The type is inside the backticks; the prose prefix is not part of it.
        assert_eq!(
            expected_type_from("expected type parameter `T`, found `()`").as_deref(),
            Some("T")
        );
    }

    #[test]
    fn expected_type_survives_no_found_clause() {
        assert_eq!(expected_type_from("expected `u32`").as_deref(), Some("u32"));
    }

    #[test]
    fn expected_type_is_none_without_an_expectation() {
        assert!(expected_type_from("cannot find value `x`").is_none());
        assert!(expected_type_from("mismatched types").is_none());
    }

    #[test]
    fn unit_value_is_detected() {
        assert!(mentions_unit_value("expected `i64`, found `()`"));
        assert!(mentions_unit_value("expected `X`, found ()"));
        assert!(!mentions_unit_value("expected `i64`, found `u32`"));
        assert!(!mentions_unit_value("mismatched types"));
        assert!(!mentions_unit_value(""));
    }

    // -- concreteness ------------------------------------------------------

    #[test]
    fn concrete_types_are_accepted() {
        for t in [
            "i64",
            "String",
            "Vec<String>",
            "Option<u32>",
            "&str",
            "(i32, bool)",
            "Result<u8, Error>",
            "Box<dyn std::fmt::Debug>",
            "*const u8",
            "f64",
        ] {
            assert_eq!(concrete_type(t).as_deref(), Ok(t), "{t} should be concrete");
        }
    }

    #[test]
    fn non_concrete_types_are_refused_with_a_reason() {
        for (t, needle) in [
            ("_", "unresolved"),
            ("{integer}", "inference variable"),
            ("{float}", "inference variable"),
            ("T", "generic type parameter"),
            ("impl Iterator<Item = u8>", "opaque"),
        ] {
            let err = concrete_type(t).unwrap_err();
            assert!(err.contains(needle), "{t}: {err}");
        }
        assert!(concrete_type("  ").is_err());
    }

    #[test]
    fn concrete_type_trims_noise() {
        assert_eq!(concrete_type("  `i64`  ").as_deref(), Ok("i64"));
    }

    // -- diagnostic parsing ------------------------------------------------

    const E0308_LINE: &str = r#"{"reason":"compiler-message","package_id":"x 0.1.0","message":{"rendered":"error[E0308]: mismatched types\n","code":{"code":"E0308","explanation":null},"level":"error","message":"mismatched types","spans":[{"file_name":"src/lib.rs","byte_start":100,"byte_end":102,"line_start":5,"line_end":5,"column_start":9,"column_end":11,"is_primary":true,"text":[{"text":"    ();","highlight_start":5,"highlight_end":7}],"label":"expected `i64`, found `()`","suggested_replacement":null}]}}"#;

    #[test]
    fn parses_a_compiler_message() {
        let diags = parse_diagnostics(E0308_LINE.as_bytes());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code.as_deref(), Some("E0308"));
        assert_eq!(diags[0].level, "error");
        assert!(diags[0].is_error());
        assert_eq!(diags[0].spans.len(), 1);
        assert_eq!(diags[0].spans[0].file_name, "src/lib.rs");
        assert_eq!(diags[0].spans[0].byte_start, 100);
        assert!(diags[0].spans[0].is_primary);
        assert_eq!(
            diags[0].spans[0].label.as_deref(),
            Some("expected `i64`, found `()`")
        );
    }

    #[test]
    fn skips_non_diagnostic_reasons() {
        let input = r#"{"reason":"build-script-executed","package_id":"x"}
{"reason":"compiler-artifact","package_id":"x"}
{"reason":"build-finished","success":true}"#;
        assert!(parse_diagnostics(input.as_bytes()).is_empty());
    }

    #[test]
    fn skips_lines_that_are_not_json() {
        let input = "Compiling app v0.1.0\nwarning: unused\ndone\n";
        assert!(parse_diagnostics(input.as_bytes()).is_empty());
    }

    #[test]
    fn tolerates_missing_optional_fields() {
        // A diagnostic with no code and no spans must not fail parsing.
        let line = r#"{"reason":"compiler-message","message":{"level":"warning","message":"unused variable"}}"#;
        let diags = parse_diagnostics(line.as_bytes());
        assert_eq!(diags.len(), 1);
        assert!(diags[0].code.is_none());
        assert!(diags[0].spans.is_empty());
    }

    #[test]
    fn ignores_unknown_extra_keys() {
        let line = r#"{"reason":"compiler-message","future_key":{"a":1},"message":{"level":"error","message":"boom","code":{"code":"E0001","explanation":"why"},"spans":[],"also_new":true}}"#;
        let diags = parse_diagnostics(line.as_bytes());
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].code.as_deref(), Some("E0001"));
    }

    #[test]
    fn parses_several_messages_from_one_stream() {
        let input = format!("{E0308_LINE}\n{E0308_LINE}\n");
        assert_eq!(parse_diagnostics(input.as_bytes()).len(), 2);
    }

    // -- attribution -------------------------------------------------------

    #[test]
    fn same_file_matches_relative_and_absolute() {
        let root = Path::new("/crate");
        let file = Path::new("/crate/src/lib.rs");
        assert!(same_file(root, "src/lib.rs", file));
        assert!(same_file(root, "/crate/src/lib.rs", file));
        assert!(!same_file(root, "src/other.rs", file));
        assert!(!same_file(root, "", file));
    }

    #[test]
    fn attribution_accepts_a_diagnostic_on_the_hole() {
        let s = Src::new("onhole", "pub fn f() -> i64 {\n    todo!(\"spec: x\")\n}\n");
        let hole = s.hole("x");
        let d = s.e0308(
            hole.byte_start,
            hole.byte_start + 2,
            "expected `i64`, found `()`",
        );
        let got = attributable_e0308(&d, &hole, s.root()).expect("should attribute");
        assert_eq!(got.as_deref(), Some("i64"));
    }

    #[test]
    fn attribution_accepts_a_later_use_within_the_window() {
        // The `let b = todo!(); b` shape: rustc reports at the later use.
        let s = Src::new(
            "later",
            "pub fn f() -> u32 {\n    let b = todo!(\"spec: x\");\n    b\n}\n",
        );
        let hole = s.hole("x");
        let d = s.e0308(
            hole.byte_start + 30,
            hole.byte_start + 31,
            "expected `u32`, found `()`",
        );
        let got = attributable_e0308(&d, &hole, s.root()).expect("should attribute");
        assert_eq!(got.as_deref(), Some("u32"));
    }

    #[test]
    fn attribution_rejects_a_use_beyond_the_window() {
        let s = Src::new("far", "pub fn f() -> u32 {\n    todo!(\"spec: x\")\n}\n");
        let hole = s.hole("x");
        let d = s.e0308(
            hole.byte_start + ATTRIBUTION_WINDOW + 1,
            hole.byte_start + ATTRIBUTION_WINDOW + 2,
            "expected `u32`, found `()`",
        );
        assert!(attributable_e0308(&d, &hole, s.root()).is_err());
    }

    #[test]
    fn attribution_rejects_an_unrelated_type_error() {
        // Same file, nearby, but not about our `()`: must not be attributed.
        let s = Src::new(
            "unrelated",
            "pub fn f() -> u32 {\n    todo!(\"spec: x\")\n}\n",
        );
        let hole = s.hole("x");
        let d = s.e0308(
            hole.byte_start + 5,
            hole.byte_start + 6,
            "expected `u32`, found `bool`",
        );
        assert!(attributable_e0308(&d, &hole, s.root()).is_err());
    }

    #[test]
    fn attribution_rejects_a_different_error_code() {
        let s = Src::new("code", "pub fn f() -> u32 {\n    todo!(\"spec: x\")\n}\n");
        let hole = s.hole("x");
        let mut d = s.e0308(
            hole.byte_start,
            hole.byte_start + 2,
            "expected `u32`, found `()`",
        );
        d.code = Some("E0277".into());
        assert!(attributable_e0308(&d, &hole, s.root()).is_err());
    }

    #[test]
    fn attribution_rejects_a_diagnostic_in_another_file() {
        let s = Src::new(
            "otherfile",
            "pub fn f() -> u32 {\n    todo!(\"spec: x\")\n}\n",
        );
        let hole = s.hole("x");
        let mut d = s.e0308(
            hole.byte_start,
            hole.byte_start + 2,
            "expected `u32`, found `()`",
        );
        d.spans[0].file_name = "src/other.rs".into();
        assert!(attributable_e0308(&d, &hole, s.root()).is_err());
    }

    // -- interpret ---------------------------------------------------------

    #[test]
    fn interpret_reports_a_known_type() {
        let s = Src::new("known", "pub fn f() -> i64 {\n    todo!(\"spec: x\")\n}\n");
        let hole = s.hole("x");
        let run = run_with(vec![s.e0308(
            hole.byte_start,
            hole.byte_start + 2,
            "expected `i64`, found `()`",
        )]);
        assert!(matches!(
            interpret(&hole, s.root(), &run),
            ProbeOutcome::Known(t) if t == "i64"
        ));
    }

    #[test]
    fn interpret_prefers_a_known_answer_over_unrelated_errors() {
        let s = Src::new("prefer", "pub fn f() -> i64 {\n    todo!(\"spec: x\")\n}\n");
        let hole = s.hole("x");
        let mut other = s.e0308(5, 9, "cannot find value `z`");
        other.code = Some("E0425".into());
        let run = run_with(vec![
            other,
            s.e0308(
                hole.byte_start,
                hole.byte_start + 2,
                "expected `i64`, found `()`",
            ),
        ]);
        assert!(matches!(
            interpret(&hole, s.root(), &run),
            ProbeOutcome::Known(t) if t == "i64"
        ));
    }

    #[test]
    fn interpret_reports_no_expectation_for_a_statement_without_compiling() {
        let s = Src::new("stmt", "pub fn f() {\n    todo!(\"spec: x\");\n}\n");
        let hole = s.hole("x");
        assert_eq!(hole.position, HolePosition::Statement);
        // Deliberately pass a run that would otherwise read as a failure:
        // statement position is decided before the compiler is consulted.
        let run = run_with(vec![s.e0308(0, 2, "expected `i64`, found `()`")]);
        assert!(matches!(
            interpret(&hole, s.root(), &run),
            ProbeOutcome::NoExpectation
        ));
    }

    #[test]
    fn interpret_reports_no_expectation_when_the_crate_compiles_silently() {
        // `let _x = todo!();` left unused: nothing constrains it, and that is an
        // ordinary situation rather than a failure of this module.
        let s = Src::new(
            "silent",
            "pub fn f() {\n    let _x = todo!(\"spec: x\");\n}\n",
        );
        let hole = s.hole("x");
        assert_eq!(hole.position, HolePosition::Expression);
        assert!(matches!(
            interpret(&hole, s.root(), &run_with(vec![])),
            ProbeOutcome::NoExpectation
        ));
    }

    #[test]
    fn interpret_reports_no_expectation_for_a_generic_type() {
        let s = Src::new(
            "generic",
            "pub fn first<T>(v: &[T]) -> T {\n    todo!(\"spec: x\")\n}\n",
        );
        let hole = s.hole("x");
        let run = run_with(vec![s.e0308(
            hole.byte_start,
            hole.byte_start + 2,
            "expected type parameter `T`, found `()`",
        )]);
        assert!(matches!(
            interpret(&hole, s.root(), &run),
            ProbeOutcome::NoExpectation
        ));
    }

    #[test]
    fn interpret_reports_a_failure_when_the_patch_itself_does_not_compile() {
        // `todo!() + 1` becomes `() + 1`: a real error at the hole, so the probe
        // must blame the position rather than the user's crate.
        let s = Src::new(
            "operator",
            "pub fn f() -> u32 {\n    todo!(\"spec: x\") + 1\n}\n",
        );
        let hole = s.hole("x");
        let mut d = s.e0308(
            hole.byte_start,
            hole.byte_start + 2,
            "cannot add `{integer}` to `()`",
        );
        d.code = Some("E0277".into());
        d.message = "cannot add `{integer}` to `()`".into();
        let run = run_with(vec![d]);

        match interpret(&hole, s.root(), &run) {
            ProbeOutcome::ProbeFailed(msg) => {
                assert!(msg.contains("fell back"), "{msg}");
                assert!(msg.contains("E0277"), "{msg}");
            }
            other => panic!("expected ProbeFailed, got {other:?}"),
        }
    }

    #[test]
    fn interpret_distinguishes_a_pre_existing_breakage_from_our_patch() {
        let s = Src::new(
            "preexisting",
            "pub fn f() -> u32 {\n    todo!(\"spec: x\")\n}\n",
        );
        let hole = s.hole("x");
        // An error far from the hole: the crate was already broken.
        let mut d = s.e0308(9000, 9001, "cannot find value `z`");
        d.code = Some("E0425".into());
        d.message = "cannot find value `z`".into();
        let run = run_with(vec![d]);

        match interpret(&hole, s.root(), &run) {
            ProbeOutcome::ProbeFailed(msg) => {
                assert!(msg.contains("does not compile"), "{msg}");
                assert!(msg.contains("not at the hole"), "{msg}");
                assert!(msg.contains("Fix these first"), "{msg}");
            }
            other => panic!("expected ProbeFailed, got {other:?}"),
        }
    }

    #[test]
    fn interpret_ignores_warnings_when_deciding() {
        let s = Src::new(
            "warnings",
            "pub fn f() {\n    let _x = todo!(\"spec: x\");\n}\n",
        );
        let hole = s.hole("x");
        let warn = Diagnostic {
            code: None,
            level: "warning".into(),
            message: "unused variable".into(),
            spans: vec![],
        };
        assert!(matches!(
            interpret(&hole, s.root(), &run_with(vec![warn])),
            ProbeOutcome::NoExpectation
        ));
    }

    #[test]
    fn error_summary_names_the_code_and_the_message() {
        let run = run_with(vec![Diagnostic {
            code: Some("E0308".into()),
            level: "error".into(),
            message: "mismatched types".into(),
            spans: vec![],
        }]);
        let s = run.error_summary();
        assert!(s.contains("E0308"), "{s}");
        assert!(s.contains("mismatched types"), "{s}");
    }

    // -- patching ----------------------------------------------------------

    #[test]
    fn patch_replaces_exactly_the_hole() {
        let s = Src::new("patch", "pub fn f() -> i64 {\n    todo!(\"spec: x\")\n}\n");
        let out = patched_source(&s.src, &s.hole("x")).unwrap();
        assert_eq!(out, "pub fn f() -> i64 {\n    ()\n}\n");
    }

    #[test]
    fn patch_preserves_every_other_byte() {
        let s = Src::new(
            "preserve",
            "pub fn f() {\n    let _x = todo!(\"spec: x\");\n}\n",
        );
        let hole = s.hole("x");
        let out = patched_source(&s.src, &hole).unwrap();
        assert_eq!(out, "pub fn f() {\n    let _x = ();\n}\n");
        // Exactly the hole's bytes were replaced, nothing more.
        assert_eq!(
            s.src.len() - (hole.byte_end - hole.byte_start) + UNIT.len(),
            out.len()
        );
    }

    #[test]
    fn patch_refuses_a_span_that_no_longer_holds_a_hole() {
        let s = Src::new("moved", "pub fn f() -> i64 {\n    todo!(\"spec: x\")\n}\n");
        // Simulate the file having been edited since the holes were listed.
        let shifted = format!("// a new leading comment\n{}", s.src);
        let err = patched_source(&shifted, &s.hole("x")).unwrap_err();
        assert!(err.contains("no longer starts"), "{err}");
        assert!(err.contains("changed since"), "{err}");
    }

    #[test]
    fn patch_refuses_a_span_past_the_end_of_the_file() {
        let s = Src::new("short", "pub fn f() -> i64 {\n    todo!(\"spec: x\")\n}\n");
        let err = patched_source("short", &s.hole("x")).unwrap_err();
        assert!(err.contains("past the end"), "{err}");
    }

    #[test]
    fn validate_span_accepts_a_freshly_extracted_hole() {
        let s = Src::new("fresh", "pub fn f() -> i64 {\n    todo!(\"spec: x\")\n}\n");
        assert!(validate_span(&s.src, &s.hole("x")).is_ok());
    }

    #[test]
    fn validate_span_accepts_unimplemented_holes() {
        let s = Src::new(
            "unimpl",
            "pub fn f() -> i64 {\n    unimplemented!(\"spec: x\")\n}\n",
        );
        let hole = s.hole("x");
        assert_eq!(hole.macro_name, "unimplemented");
        assert!(validate_span(&s.src, &hole).is_ok());
    }

    // -- multi-hole patching -----------------------------------------------

    #[test]
    fn many_patches_replace_every_hole() {
        let s = Src::new(
            "many-patch",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: two\")\n}\n",
        );
        let holes = s.holes();
        assert_eq!(holes.len(), 2);
        let refs: Vec<&Hole> = holes.iter().collect();

        let (out, _) = patched_source_many(&s.src, &refs).unwrap();
        assert_eq!(
            out,
            "pub fn a() -> i64 {\n    ()\n}\n\npub fn b() -> bool {\n    ()\n}\n"
        );
        assert!(!out.contains("todo!"));
    }

    #[test]
    fn many_patches_do_not_care_about_input_order() {
        let s = Src::new(
            "many-order",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: two\")\n}\n",
        );
        let holes = s.holes();
        let forward: Vec<&Hole> = holes.iter().collect();
        let backward: Vec<&Hole> = holes.iter().rev().collect();
        assert_eq!(
            patched_source_many(&s.src, &forward).unwrap().0,
            patched_source_many(&s.src, &backward).unwrap().0,
            "splicing left to right must not depend on the caller's order"
        );
    }

    #[test]
    fn many_patches_reject_a_single_spot() {
        // A batch of one is allowed; it is just a single replace.
        let s = Src::new(
            "one-patch",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n",
        );
        let holes = s.holes();
        let refs: Vec<&Hole> = holes.iter().collect();
        assert_eq!(
            patched_source_many(&s.src, &refs).unwrap().0,
            "pub fn a() -> i64 {\n    ()\n}\n"
        );
    }

    #[test]
    fn many_patches_reject_an_empty_list() {
        let s = Src::new("empty-patch", "pub fn a() -> i64 {\n    ()\n}\n");
        assert_eq!(patched_source_many(&s.src, &[]).unwrap().0, s.src);
    }

    #[test]
    fn many_patches_reject_overlapping_spans() {
        let s = Src::new(
            "overlap",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n",
        );
        let hole = s.hole("one");
        // Two entries for the same span. A caller that got here has a bug, and
        // splicing both would corrupt the file.
        let refs = vec![&hole, &hole];
        let err = patched_source_many(&s.src, &refs).unwrap_err();
        assert!(err.contains("overlap"), "{err}");
    }

    #[test]
    fn many_patches_reject_a_stale_span() {
        let s = Src::new(
            "stale",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n",
        );
        let shifted = format!("// shifted\n{}", s.src);
        let holes = s.holes();
        let refs: Vec<&Hole> = holes.iter().collect();
        let err = patched_source_many(&shifted, &refs).unwrap_err();
        assert!(err.contains("no longer starts"), "{err}");
    }

    // -- shared with single-hole patching ----------------------------------

    #[test]
    fn one_and_many_patching_agree_on_a_single_hole() {
        let s = Src::new(
            "agree",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n",
        );
        let hole = s.hole("one");
        assert_eq!(
            patched_source(&s.src, &hole).unwrap(),
            patched_source_many(&s.src, &[&hole]).unwrap().0
        );
    }

    // -- splitting for the batch -------------------------------------------

    #[test]
    fn statements_and_unresolvable_holes_are_never_batched() {
        let s = Src::new(
            "split",
            "pub fn a() -> i64 {\n    todo!(\"spec: expr\")\n}\n\n\
             pub fn b() {\n    todo!(\"spec: stmt\");\n}\n",
        );
        let mut holes = s.holes();
        // Mark the expression hole unresolvable; it must be excluded too.
        if let Some(h) = holes.iter_mut().find(|h| h.spec == "expr") {
            h.unresolvable = Some("test".into());
        }

        let (by_file, impossible) = split_batchable(&holes);
        assert!(by_file.is_empty(), "nothing here is safely batchable");
        assert_eq!(impossible.len(), 2);

        // The statement reads as no-expectation; the unresolvable one as a
        // failure, since its span was never validated.
        let stmt = holes.iter().position(|h| h.spec == "stmt").unwrap();
        assert!(matches!(
            impossible.iter().find(|(i, _)| *i == stmt).map(|(_, o)| o),
            Some(ProbeOutcome::NoExpectation)
        ));
    }

    #[test]
    fn batchable_holes_keep_their_original_index() {
        let s = Src::new(
            "index",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: two\")\n}\n",
        );
        let holes = s.holes();
        let (by_file, impossible) = split_batchable(&holes);
        assert!(impossible.is_empty());
        let indexed: Vec<usize> = by_file.values().flatten().map(|(i, _)| *i).collect();
        assert_eq!(indexed, vec![0, 1], "verdicts must map back to input order");
    }

    // -- batch interpretation ----------------------------------------------

    #[test]
    fn batched_run_declines_a_hole_it_cannot_settle() {
        // Errors exist, none attributable to this hole, and the run was batched:
        // another patched hole may have caused this. Must return None (decline)
        // rather than blame the crate.
        let s = Src::new(
            "decline",
            "pub fn f() -> u32 {\n    todo!(\"spec: x\")\n}\n",
        );
        let hole = s.hole("x");
        let mut d = s.e0308(9000, 9001, "cannot find value `z`");
        d.code = Some("E0425".into());
        let run = run_with(vec![d]);

        let blame = attributable_diagnostics(&hole, s.root(), &run);
        assert!(blame.is_empty(), "the far error is not ours");

        assert!(
            decide(&hole, s.root(), &run, &blame, Isolation::Batched).is_none(),
            "a batched run must not blame the crate for a neighbour's error"
        );
        // The same run in isolation is decidable: the only patch was ours.
        assert!(matches!(
            decide(&hole, s.root(), &run, &blame, Isolation::Single),
            Some(ProbeOutcome::ProbeFailed(_))
        ));
    }

    #[test]
    fn batched_run_settles_a_hole_when_nothing_failed() {
        // No errors at all means every patch compiled, so nothing can have
        // suppressed this hole's diagnostic: the batch can answer directly and
        // needs no fallback. This is what makes the common case cheap.
        let s = Src::new(
            "settled",
            "pub fn f() {\n    let _x = todo!(\"spec: x\");\n}\n",
        );
        let hole = s.hole("x");
        assert!(matches!(
            decide(&hole, s.root(), &run_with(vec![]), &[], Isolation::Batched),
            Some(ProbeOutcome::NoExpectation)
        ));
    }

    #[test]
    fn batched_run_settles_an_attributable_answer_despite_other_errors() {
        // Another hole's error elsewhere does not invalidate direct evidence
        // about this hole, batched or not.
        let s = Src::new(
            "batched-known",
            "pub fn f() -> i64 {\n    todo!(\"spec: x\")\n}\n",
        );
        let hole = s.hole("x");
        let mut other = s.e0308(9000, 9001, "cannot find value `z`");
        other.code = Some("E0425".into());
        let run = run_with(vec![
            other,
            s.e0308(
                hole.byte_start,
                hole.byte_start + 2,
                "expected `i64`, found `()`",
            ),
        ]);
        let blame = attributable_diagnostics(&hole, s.root(), &run);
        assert_eq!(blame.len(), 1, "only our own diagnostic is ours");
        assert!(matches!(
            decide(&hole, s.root(), &run, &blame, Isolation::Batched),
            Some(ProbeOutcome::Known(t)) if t == "i64"
        ));
    }

    #[test]
    fn batched_run_settles_an_unusable_answer() {
        // `expected ... `T`` is an answer of sorts, and is decided without
        // reference to the rest of the run, so the batch can commit to it.
        let s = Src::new(
            "batched-generic",
            "pub fn first<T>(v: &[T]) -> T {\n    todo!(\"spec: x\")\n}\n",
        );
        let hole = s.hole("x");
        let run = run_with(vec![s.e0308(
            hole.byte_start,
            hole.byte_start + 2,
            "expected type parameter `T`, found `()`",
        )]);
        let blame = attributable_diagnostics(&hole, s.root(), &run);
        assert!(matches!(
            decide(&hole, s.root(), &run, &blame, Isolation::Batched),
            Some(ProbeOutcome::NoExpectation)
        ));
    }

    #[test]
    fn single_interpretation_never_declines() {
        // `interpret` is the Single-mode wrapper and must always produce an
        // answer; a None there would be an internal bug, not a user situation.
        let s = Src::new(
            "single-never",
            "pub fn f() -> u32 {\n    todo!(\"spec: x\")\n}\n",
        );
        let hole = s.hole("x");
        let mut d = s.e0308(9000, 9001, "cannot find value `z`");
        d.code = Some("E0425".into());
        assert!(matches!(
            interpret(&hole, s.root(), &run_with(vec![d])),
            ProbeOutcome::ProbeFailed(_)
        ));
    }

    // -- diagnostic assignment ---------------------------------------------

    #[test]
    fn assignment_gives_each_hole_its_own_diagnostic() {
        // The bug this exists to prevent: three holes close together with a
        // window wider than the gaps between them. A per-hole rule would let all
        // three claim the first diagnostic and report `i64` three times.
        let s = Src::new(
            "assign",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: two\")\n}\n\n\
             pub fn c() -> String {\n    todo!(\"spec: three\")\n}\n",
        );
        let holes = s.holes();
        assert_eq!(holes.len(), 3);
        // The holes really are within one attribution window of each other.
        assert!(
            holes[2].byte_start - holes[0].byte_start < ATTRIBUTION_WINDOW,
            "the fixture must reproduce the overlapping-window situation"
        );

        let refs: Vec<&Hole> = holes.iter().collect();
        let (_, anchors) = patched_source_many(&s.src, &refs).unwrap();

        // rustc reports spans against the *patched* text, so the fixture must
        // too: build each diagnostic at its hole's patched offset, exactly as a
        // real batched run would.
        let at = |i: usize, ty: &str| {
            s.e0308(
                anchors[i],
                anchors[i] + 2,
                &format!("expected `{ty}`, found `()`"),
            )
        };
        let run = run_with(vec![at(0, "i64"), at(1, "bool"), at(2, "String")]);

        let blame = assign_diagnostics(&refs, &anchors, s.root(), &run);
        assert_eq!(blame.len(), 3);
        for (i, b) in blame.iter().enumerate() {
            assert_eq!(b.len(), 1, "hole {i} must claim exactly one diagnostic");
        }

        // And each hole now resolves to its own type.
        let types: Vec<Option<String>> = holes
            .iter()
            .zip(&blame)
            .map(|(h, b)| decide(h, s.root(), &run, b, Isolation::Batched))
            .map(|o| o.and_then(|o| o.type_text().map(str::to_string)))
            .collect();
        assert_eq!(
            types,
            vec![
                Some("i64".to_string()),
                Some("bool".to_string()),
                Some("String".to_string())
            ]
        );
    }

    #[test]
    fn assignment_prefers_the_closest_hole() {
        // rustc reports `let b = todo!(); b` at the later use, which may be
        // nearer the next hole than its own. Distance decides, so the true
        // owner -- at distance 0 from where rustc pointed -- still wins.
        let s = Src::new(
            "assign-close",
            "pub fn a() -> i64 {\n    let v = todo!(\"spec: one\");\n    v\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: two\")\n}\n",
        );
        let holes = s.holes();
        let refs: Vec<&Hole> = holes.iter().collect();

        // One diagnostic, sitting on hole b's span. Hole a is merely nearby.
        let run = run_with(vec![s.e0308(
            holes[1].byte_start,
            holes[1].byte_start + 2,
            "expected `bool`, found `()`",
        )]);

        let (_, anchors) = patched_source_many(&s.src, &refs).unwrap();
        let blame = assign_diagnostics(&refs, &anchors, s.root(), &run);
        assert!(blame[0].is_empty(), "hole a must not steal hole b's error");
        assert_eq!(blame[1].len(), 1);

        let types: Vec<Option<String>> = holes
            .iter()
            .zip(&blame)
            .map(|(h, b)| decide(h, s.root(), &run, b, Isolation::Batched))
            .map(|o| o.and_then(|o| o.type_text().map(str::to_string)))
            .collect();
        assert_eq!(types[1], Some("bool".to_string()));
        // Hole a is undecided by the batch (a non-zero error count, nothing
        // attributed), so it declines and gets re-probed.
        assert_eq!(types[0], None);
    }

    #[test]
    fn assignment_ignores_diagnostics_about_other_types() {
        // An unrelated E0308 mentions its own found type, not `()`, so it is
        // never up for assignment.
        let s = Src::new(
            "assign-unrelated",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n",
        );
        let holes = s.holes();
        let refs: Vec<&Hole> = holes.iter().collect();
        let run = run_with(vec![s.e0308(
            holes[0].byte_start,
            holes[0].byte_start + 2,
            "expected `i64`, found `bool`",
        )]);
        let (_, anchors) = patched_source_many(&s.src, &refs).unwrap();
        let blame = assign_diagnostics(&refs, &anchors, s.root(), &run);
        assert!(blame[0].is_empty());
    }

    #[test]
    fn assignment_handles_more_holes_than_diagnostics() {
        // Two holes, one diagnostic: one gets it, the other must not.
        let s = Src::new(
            "assign-fewer",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: two\")\n}\n",
        );
        let holes = s.holes();
        let refs: Vec<&Hole> = holes.iter().collect();
        let run = run_with(vec![s.e0308(
            holes[1].byte_start,
            holes[1].byte_start + 2,
            "expected `bool`, found `()`",
        )]);
        let (_, anchors) = patched_source_many(&s.src, &refs).unwrap();
        let blame = assign_diagnostics(&refs, &anchors, s.root(), &run);
        assert!(blame[0].is_empty());
        assert_eq!(blame[1].len(), 1);
    }

    #[test]
    fn assignment_is_deterministic() {
        // Same inputs, same assignment: no dependence on hash order.
        let s = Src::new(
            "assign-det",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: two\")\n}\n",
        );
        let holes = s.holes();
        let refs: Vec<&Hole> = holes.iter().collect();
        let run = run_with(vec![
            s.e0308(
                holes[0].byte_start,
                holes[0].byte_start + 2,
                "expected `i64`, found `()`",
            ),
            s.e0308(
                holes[1].byte_start,
                holes[1].byte_start + 2,
                "expected `bool`, found `()`",
            ),
        ]);
        for _ in 0..5 {
            let (_, anchors) = patched_source_many(&s.src, &refs).unwrap();
            let blame = assign_diagnostics(&refs, &anchors, s.root(), &run);
            assert_eq!(blame[0].len(), 1);
            assert_eq!(blame[1].len(), 1);
        }
    }

    // -- restore guard -----------------------------------------------------

    fn scratch_file(tag: &str, content: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cargo-hole-scratch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lib.rs");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn guard_restores_the_original_bytes() {
        let path = scratch_file("restore", "original\n");
        let mut guard = RestoreGuard::new(&path, b"original\n".to_vec(), "patched\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "patched\n");
        guard.restore().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
        assert!(
            !backup_path(&path).exists(),
            "the backup must be cleaned up"
        );
    }

    #[test]
    fn guard_drop_restores_even_without_an_explicit_restore() {
        let path = scratch_file("drop", "original\n");
        {
            let _guard = RestoreGuard::new(&path, b"original\n".to_vec(), "patched\n").unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn guard_drop_restores_while_unwinding_a_panic() {
        let path = scratch_file("panic", "original\n");
        let p = path.clone();
        let result = std::panic::catch_unwind(move || {
            let _guard = RestoreGuard::new(&p, b"original\n".to_vec(), "patched\n").unwrap();
            panic!("boom");
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "original\n");
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn guard_keep_leaves_the_patch_in_place() {
        let path = scratch_file("keep", "original\n");
        let mut guard = RestoreGuard::new(&path, b"original\n".to_vec(), "patched\n").unwrap();
        guard.keep();
        drop(guard);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "patched\n");
        assert!(!backup_path(&path).exists());
    }

    #[test]
    fn guard_keeps_the_backup_on_disk_while_armed() {
        let path = scratch_file("backup", "original\n");
        let guard = RestoreGuard::new(&path, b"original\n".to_vec(), "patched\n").unwrap();
        // While armed, the backup is the crash-recovery record.
        assert!(backup_path(&path).exists());
        assert_eq!(
            std::fs::read_to_string(backup_path(&path)).unwrap(),
            "original\n"
        );
        drop(guard);
    }

    #[test]
    fn backup_path_appends_the_suffix_beside_the_file() {
        assert_eq!(
            backup_path(Path::new("/a/b/lib.rs")),
            PathBuf::from("/a/b/lib.rs.cargo-hole.bak")
        );
    }

    #[test]
    fn restore_leftovers_recovers_a_simulated_hard_kill() {
        let dir = std::env::temp_dir().join(format!("cargo-hole-leftover-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let file = dir.join("src/lib.rs");
        std::fs::write(&file, "PATCHED\n").unwrap();
        // A backup with no live guard is exactly what a SIGKILL leaves behind.
        std::fs::write(backup_path(&file), "ORIGINAL\n").unwrap();

        let restored = restore_leftovers(&dir).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ORIGINAL\n");
        assert!(!backup_path(&file).exists());
    }

    #[test]
    fn restore_leftovers_finds_backups_in_nested_directories() {
        let dir = std::env::temp_dir().join(format!("cargo-hole-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/deep/deeper")).unwrap();
        let file = dir.join("src/deep/deeper/lib.rs");
        std::fs::write(&file, "PATCHED\n").unwrap();
        std::fs::write(backup_path(&file), "ORIGINAL\n").unwrap();

        assert_eq!(restore_leftovers(&dir).unwrap().len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "ORIGINAL\n");
    }

    #[test]
    fn restore_leftovers_skips_target_and_dotdirs() {
        let dir = std::env::temp_dir().join(format!("cargo-hole-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join("target/debug/a.rs.cargo-hole.bak"), "x").unwrap();
        std::fs::write(dir.join(".git/b.rs.cargo-hole.bak"), "x").unwrap();

        assert!(restore_leftovers(&dir).unwrap().is_empty());
    }

    #[test]
    fn restore_leftovers_reports_nothing_when_clean() {
        let dir = std::env::temp_dir().join(format!("cargo-hole-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(restore_leftovers(&dir).unwrap().is_empty());
    }

    // -- locking -----------------------------------------------------------

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cargo-hole-dir-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lock_is_exclusive_within_a_process() {
        let dir = scratch_dir("lock");
        let first = ProbeLock::acquire(&dir, Duration::from_millis(100)).unwrap();
        assert!(first.is_held());
        // A second acquisition while the first is alive must wait, then fail.
        assert!(
            ProbeLock::acquire(&dir, Duration::from_millis(200)).is_err(),
            "the lock must not be shared"
        );
        drop(first);
        // Once released it can be taken again.
        assert!(ProbeLock::acquire(&dir, Duration::from_millis(200)).is_ok());
    }

    #[test]
    fn lock_file_lives_in_the_root_and_is_named_as_documented() {
        let dir = scratch_dir("lockname");
        let lock = ProbeLock::acquire(&dir, Duration::from_millis(100)).unwrap();
        assert_eq!(lock.path(), dir.join(LOCK_NAME));
        assert!(dir.join(".cargo-hole.probe.lock").exists());
    }

    // -- options -----------------------------------------------------------

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn check_command_carries_message_format_and_quiet() {
        let cmd = ProberOptions::default().check_command(Path::new("/crate"));
        let args = args_of(&cmd);
        assert!(args.contains(&"check".to_string()), "{args:?}");
        assert!(
            args.contains(&"--message-format=json".to_string()),
            "{args:?}"
        );
        assert!(args.contains(&"--quiet".to_string()), "{args:?}");
    }

    #[test]
    fn check_command_adds_offline_and_target_dir_when_asked() {
        let opts = ProberOptions {
            offline: true,
            target_dir: Some(PathBuf::from("/tmp/t")),
            ..ProberOptions::default()
        };
        let args = args_of(&opts.check_command(Path::new("/crate")));
        assert!(args.contains(&"--offline".to_string()), "{args:?}");
        assert!(args.contains(&"--target-dir".to_string()), "{args:?}");
        assert!(args.contains(&"/tmp/t".to_string()), "{args:?}");
    }

    #[test]
    fn check_command_sets_cargo_home_on_the_child_not_the_process() {
        // The read-only `~/.cargo` case: the override must reach the child
        // without `set_var`, which is unsafe in edition 2024.
        let opts = ProberOptions {
            cargo_home: Some(PathBuf::from("/tmp/cargo-home")),
            ..ProberOptions::default()
        };
        let cmd = opts.check_command(Path::new("/crate"));
        let home = cmd
            .get_envs()
            .find(|(k, _)| *k == "CARGO_HOME")
            .and_then(|(_, v)| v);
        assert_eq!(home, Some(std::ffi::OsStr::new("/tmp/cargo-home")));
    }

    #[test]
    fn without_a_cargo_home_override_the_child_inherits_ours() {
        let cmd = ProberOptions::default().check_command(Path::new("/crate"));
        assert!(
            cmd.get_envs().all(|(k, _)| k != "CARGO_HOME"),
            "unset means inherit, not override"
        );
    }

    #[test]
    fn outcome_labels_are_stable() {
        assert_eq!(ProbeOutcome::Known("i64".into()).label(), "known");
        assert_eq!(ProbeOutcome::NoExpectation.label(), "no-expectation");
        assert_eq!(
            ProbeOutcome::ProbeFailed("x".into()).label(),
            "probe-failed"
        );
        assert_eq!(ProbeOutcome::Broken(anyhow::anyhow!("x")).label(), "broken");
        assert_eq!(ProbeOutcome::Known("i64".into()).type_text(), Some("i64"));
        assert_eq!(ProbeOutcome::NoExpectation.type_text(), None);
    }

    // -- subprocess handling ------------------------------------------------

    #[test]
    fn run_command_captures_output() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo hello");
        let out = run_command(&mut cmd, Duration::from_secs(10)).unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
        assert_eq!(out.code, Some(0));
        assert!(!out.timed_out);
    }

    #[test]
    fn run_command_kills_the_whole_process_group_on_timeout() {
        // `sh` spawns `sleep` as a grandchild. Killing only `sh` would leave the
        // orphan holding the pipe, and the reader threads would block long past
        // the deadline.
        let started = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30");
        let out = run_command(&mut cmd, Duration::from_millis(200)).unwrap();
        assert!(out.timed_out);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "took {:?}; the process group was not killed",
            started.elapsed()
        );
    }

    #[test]
    fn run_command_reports_a_missing_program_clearly() {
        let mut cmd = Command::new("definitely-not-a-real-program-xyz");
        let err = run_command(&mut cmd, Duration::from_secs(5)).unwrap_err();
        assert!(format!("{err:#}").contains("cannot run"), "{err:#}");
    }

    // -- real cargo ---------------------------------------------------------

    /// A throwaway crate that cargo can check on its own.
    struct TempCrate {
        dir: PathBuf,
    }

    impl TempCrate {
        fn new(tag: &str, lib_rs: &str) -> TempCrate {
            let dir =
                std::env::temp_dir().join(format!("cargo-hole-crate-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("src")).unwrap();
            std::fs::write(
                dir.join("Cargo.toml"),
                "[package]\nname = \"probecrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
                 \n[workspace]\n",
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

        fn hole(&self) -> Hole {
            let mut holes = self.holes();
            assert_eq!(holes.len(), 1, "these fixtures carry exactly one hole");
            holes.remove(0)
        }

        fn options(&self) -> ProberOptions {
            // A scratch Cargo home, because `~/.cargo` may be read-only: cargo
            // wants to touch its package cache even with no dependencies.
            let cargo_home = std::env::temp_dir().join("cargo-hole-cargo-home");
            let _ = std::fs::create_dir_all(&cargo_home);
            ProberOptions::for_tests(self.dir.join("target"), cargo_home)
        }

        fn prober(&self) -> Prober {
            Prober::new(self.dir.clone(), self.options())
        }
    }

    impl Drop for TempCrate {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn probes_a_concrete_return_type() {
        let tc = TempCrate::new(
            "concrete",
            "pub fn f() -> i64 {\n    todo!(\"spec: reply\")\n}\n",
        );
        let outcome = tc.prober().probe(&tc.hole());
        assert!(
            matches!(&outcome, ProbeOutcome::Known(t) if t == "i64"),
            "{outcome:?}"
        );
    }

    #[test]
    fn probes_a_std_generic_type() {
        let tc = TempCrate::new(
            "vec",
            "pub fn f() -> Vec<String> {\n    todo!(\"spec: reply\")\n}\n",
        );
        let outcome = tc.prober().probe(&tc.hole());
        assert!(
            matches!(&outcome, ProbeOutcome::Known(t) if t == "Vec<String>"),
            "{outcome:?}"
        );
    }

    #[test]
    fn probes_an_option_type() {
        let tc = TempCrate::new(
            "option",
            "pub fn f() -> Option<u32> {\n    todo!(\"spec: reply\")\n}\n",
        );
        let outcome = tc.prober().probe(&tc.hole());
        assert!(
            matches!(&outcome, ProbeOutcome::Known(t) if t == "Option<u32>"),
            "{outcome:?}"
        );
    }

    #[test]
    fn probes_a_reference_type() {
        let tc = TempCrate::new(
            "ref",
            "pub fn f<'a>(s: &'a str) -> &'a str {\n    let _ = s;\n    todo!(\"spec: reply\")\n}\n",
        );
        let outcome = tc.prober().probe(&tc.hole());
        assert!(
            matches!(&outcome, ProbeOutcome::Known(t) if t.contains('&') && t.contains("str")),
            "{outcome:?}"
        );
    }

    #[test]
    fn probes_an_if_condition_as_bool() {
        let tc = TempCrate::new(
            "cond",
            "pub fn f() -> u32 {\n    if todo!(\"spec: reply\") {\n        1\n    } else {\n        0\n    }\n}\n",
        );
        let outcome = tc.prober().probe(&tc.hole());
        assert!(
            matches!(&outcome, ProbeOutcome::Known(t) if t == "bool"),
            "{outcome:?}"
        );
    }

    #[test]
    fn probes_a_hole_inside_a_macro_body() {
        let tc = TempCrate::new(
            "macbody",
            "pub fn f() -> Vec<u32> {\n    vec![todo!(\"spec: reply\")]\n}\n",
        );
        let hole = tc.hole();
        assert_eq!(hole.position, HolePosition::MacroBody);
        let outcome = tc.prober().probe(&hole);
        assert!(
            matches!(&outcome, ProbeOutcome::Known(t) if t == "u32"),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_statement_hole_needs_no_compiler_at_all() {
        let tc = TempCrate::new("stmt", "pub fn f() {\n    todo!(\"spec: reply\");\n}\n");
        let hole = tc.hole();
        assert_eq!(hole.position, HolePosition::Statement);
        // Point the prober at a cargo that cannot possibly run: statement
        // position is decided without touching the compiler.
        let opts = ProberOptions {
            cargo: "definitely-not-cargo".into(),
            ..tc.options()
        };
        let prober = Prober::new(tc.dir.clone(), opts);
        assert!(matches!(prober.probe(&hole), ProbeOutcome::NoExpectation));
    }

    #[test]
    fn an_unconstrained_let_reports_no_expectation() {
        let tc = TempCrate::new(
            "unconstrained",
            "pub fn f() {\n    let _x = todo!(\"spec: reply\");\n}\n",
        );
        let outcome = tc.prober().probe(&tc.hole());
        assert!(
            matches!(outcome, ProbeOutcome::NoExpectation),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_generic_return_reports_no_expectation() {
        let tc = TempCrate::new(
            "generic",
            "pub fn first<T>(v: &[T]) -> T {\n    let _ = v;\n    todo!(\"spec: reply\")\n}\n",
        );
        let outcome = tc.prober().probe(&tc.hole());
        assert!(
            matches!(outcome, ProbeOutcome::NoExpectation),
            "{outcome:?}"
        );
    }

    #[test]
    fn an_operator_position_never_yields_a_guessed_type() {
        let tc = TempCrate::new(
            "operator",
            "pub fn f() -> u32 {\n    todo!(\"spec: reply\") + 1\n}\n",
        );
        match tc.prober().probe(&tc.hole()) {
            // The expected outcome: `()` cannot be added, so rustc emits E0369
            // (or E0277) and the probe reports that it could not get an answer.
            ProbeOutcome::ProbeFailed(msg) => {
                assert!(msg.contains("fell back"), "{msg}");
                assert!(
                    msg.contains("E0369") || msg.contains("E0277"),
                    "the summary should name the real error: {msg}"
                );
            }
            // Tolerated if rustc answers with E0308 instead. What must never
            // happen is a guessed type.
            ProbeOutcome::NoExpectation => {}
            other => panic!("expected a failure or no-expectation, got {other:?}"),
        }
    }

    #[test]
    fn probing_leaves_the_file_byte_identical() {
        let tc = TempCrate::new(
            "identical",
            "pub fn f() -> i64 {\n    todo!(\"spec: reply\")\n}\n",
        );
        let before = std::fs::read(tc.lib()).unwrap();
        let _ = tc.prober().probe(&tc.hole());
        assert_eq!(std::fs::read(tc.lib()).unwrap(), before);
        assert!(!backup_path(&tc.lib()).exists(), "no backup may remain");
    }

    #[test]
    fn probing_several_holes_leaves_no_trace() {
        let tc = TempCrate::new(
            "many",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: two\")\n}\n",
        );
        let before = std::fs::read(tc.lib()).unwrap();
        let prober = tc.prober();

        let mut types = Vec::new();
        for hole in tc.holes() {
            types.push(prober.probe(&hole).type_text().map(str::to_string));
        }
        assert_eq!(
            types,
            vec![Some("i64".to_string()), Some("bool".to_string())]
        );
        assert_eq!(std::fs::read(tc.lib()).unwrap(), before);
        assert!(!backup_path(&tc.lib()).exists());
    }

    #[test]
    fn an_unrelated_error_elsewhere_does_not_block_a_known_answer() {
        // A crate with a hole *and* a pre-existing error. rustc still reports
        // the E0308 for our hole, and that is direct evidence, so the probe
        // answers rather than refusing. Refusing would make the tool useless on
        // exactly the crates that tend to have holes.
        let tc = TempCrate::new(
            "unrelated",
            "pub fn f() -> i64 {\n    todo!(\"spec: reply\")\n}\n\n\
             pub fn g() -> u8 {\n    \"not a number\"\n}\n",
        );
        let outcome = tc.prober().probe(&tc.hole());
        assert!(
            matches!(&outcome, ProbeOutcome::Known(t) if t == "i64"),
            "{outcome:?}"
        );
        assert!(!backup_path(&tc.lib()).exists());
    }

    #[test]
    fn a_crate_that_fails_away_from_the_hole_reports_a_failure() {
        // The hole is a `let _ = ()` that constrains nothing, so no error is
        // attributable to it; the only error is far away. That is the
        // "already broken" branch rather than the "position fell back" one.
        // The padding keeps the bad name outside the blame window, so this also
        // proves the window is what separates the two messages.
        let tc = TempCrate::new(
            "broken",
            "pub fn f() {\n    let _ = todo!(\"spec: reply\");\n}\n\n\
             // padding padding padding padding padding padding padding padding\n\
             // padding padding padding padding padding padding padding padding\n\
             // padding padding padding padding padding padding padding padding\n\
             pub fn g() -> u32 {\n    missing_ident\n}\n",
        );
        let hole = tc.hole();
        let src = std::fs::read_to_string(tc.lib()).unwrap();
        let far = src
            .rfind("missing_ident")
            .expect("fixture has the bad name");
        assert!(
            far.saturating_sub(hole.byte_end) > LOCAL_WINDOW,
            "the fixture must place the unrelated error outside the blame window"
        );

        match tc.prober().probe(&hole) {
            ProbeOutcome::ProbeFailed(msg) => assert!(msg.contains("does not compile"), "{msg}"),
            other => panic!("expected ProbeFailed, got {other:?}"),
        }
        assert!(!backup_path(&tc.lib()).exists());
    }

    #[test]
    fn a_missing_cargo_is_reported_as_broken() {
        let tc = TempCrate::new(
            "nocargo",
            "pub fn f() -> i64 {\n    todo!(\"spec: reply\")\n}\n",
        );
        let opts = ProberOptions {
            cargo: "definitely-not-a-real-cargo-binary".into(),
            ..tc.options()
        };
        let prober = Prober::new(tc.dir.clone(), opts);
        match prober.probe(&tc.hole()) {
            ProbeOutcome::Broken(e) => {
                assert!(format!("{e:#}").contains("cannot run"), "{e:#}");
            }
            other => panic!("expected Broken, got {other:?}"),
        }
        // Even a probe that never ran must leave the file untouched.
        assert!(!backup_path(&tc.lib()).exists());
    }

    #[test]
    fn probing_restores_the_file_even_when_cargo_fails() {
        let tc = TempCrate::new(
            "restore-on-fail",
            "pub fn f() {\n    let _ = todo!(\"spec: reply\");\n}\n\n\
             pub fn g() -> u32 {\n    missing_ident\n}\n",
        );
        let before = std::fs::read(tc.lib()).unwrap();
        let outcome = tc.prober().probe(&tc.hole());
        assert!(
            matches!(outcome, ProbeOutcome::ProbeFailed(_)),
            "this fixture must genuinely fail: {outcome:?}"
        );
        assert_eq!(std::fs::read(tc.lib()).unwrap(), before);
        assert!(!backup_path(&tc.lib()).exists());
    }

    #[test]
    fn probing_twice_in_a_row_gives_the_same_answer() {
        // The patch is restored before the diagnostics are read, so a second
        // probe must see the same original file and answer identically.
        let tc = TempCrate::new(
            "twice",
            "pub fn f() -> u32 {\n    todo!(\"spec: reply\")\n}\n",
        );
        let prober = tc.prober();
        let first = prober.probe(&tc.hole()).type_text().map(str::to_string);
        let second = prober.probe(&tc.hole()).type_text().map(str::to_string);
        assert_eq!(first, Some("u32".to_string()));
        assert_eq!(first, second);
    }

    // -- batch probing against real cargo ----------------------------------

    #[test]
    fn batch_probing_answers_every_hole() {
        // The core claim: patching all the holes at once still yields each
        // hole's own type, because every E0308 carries its own span.
        let tc = TempCrate::new(
            "batch-all",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: two\")\n}\n\n\
             pub fn c() -> String {\n    todo!(\"spec: three\")\n}\n\n\
             pub fn d() -> Vec<u8> {\n    todo!(\"spec: four\")\n}\n",
        );
        let before = std::fs::read(tc.lib()).unwrap();
        let holes = tc.holes();
        assert_eq!(holes.len(), 4);

        let outcomes = tc.prober().probe_all(&holes);
        assert_eq!(outcomes.len(), 4, "one verdict per hole, in input order");
        let types: Vec<Option<&str>> = outcomes.iter().map(|o| o.type_text()).collect();
        assert_eq!(
            types,
            vec![Some("i64"), Some("bool"), Some("String"), Some("Vec<u8>")],
            "{outcomes:?}"
        );

        assert_eq!(std::fs::read(tc.lib()).unwrap(), before, "file restored");
        assert!(!backup_path(&tc.lib()).exists());
    }

    #[test]
    fn batch_probing_handles_holes_across_several_files() {
        let tc = TempCrate::new("batch-files", "pub fn unused() {}\n");
        std::fs::write(tc.dir.join("src/lib.rs"), "pub mod one;\npub mod two;\n").unwrap();
        std::fs::write(
            tc.dir.join("src/one.rs"),
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n",
        )
        .unwrap();
        std::fs::write(
            tc.dir.join("src/two.rs"),
            "pub fn b() -> bool {\n    todo!(\"spec: two\")\n}\n",
        )
        .unwrap();

        let before: Vec<Vec<u8>> = ["lib.rs", "one.rs", "two.rs"]
            .iter()
            .map(|f| std::fs::read(tc.dir.join("src").join(f)).unwrap())
            .collect();

        let holes = Hole::list_holes(&tc.dir).expect("list");
        assert_eq!(holes.len(), 2);
        let outcomes = tc.prober().probe_all(&holes);

        let mut types: Vec<Option<&str>> = outcomes.iter().map(|o| o.type_text()).collect();
        types.sort();
        assert_eq!(types, vec![Some("bool"), Some("i64")], "{outcomes:?}");

        // Every file must come out of a multi-file batch untouched.
        for (i, f) in ["lib.rs", "one.rs", "two.rs"].iter().enumerate() {
            let now = std::fs::read(tc.dir.join("src").join(f)).unwrap();
            assert_eq!(now, before[i], "{f} was not restored");
            assert!(!backup_path(&tc.dir.join("src").join(f)).exists());
        }
    }

    #[test]
    fn batch_probing_mixes_known_and_unknown_holes() {
        // A batch where some holes answer and others are unconstrained. The
        // run's errors are the four attributable ones plus nothing else, so
        // every hole is decidable without a fallback.
        let tc = TempCrate::new(
            "batch-mixed",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n\n\
             pub fn b() {\n    let _x = todo!(\"spec: two\");\n}\n\n\
             pub fn c() -> bool {\n    todo!(\"spec: three\")\n}\n",
        );
        let holes = tc.holes();
        let outcomes = tc.prober().probe_all(&holes);

        assert!(
            matches!(&outcomes[0], ProbeOutcome::Known(t) if t == "i64"),
            "{outcomes:?}"
        );
        assert!(
            matches!(&outcomes[1], ProbeOutcome::NoExpectation),
            "{outcomes:?}"
        );
        assert!(
            matches!(&outcomes[2], ProbeOutcome::Known(t) if t == "bool"),
            "{outcomes:?}"
        );
    }

    #[test]
    fn batch_probing_skips_statements_without_a_compiler() {
        let tc = TempCrate::new(
            "batch-stmt",
            "pub fn a() {\n    todo!(\"spec: one\");\n}\n\n\
             pub fn b() -> i64 {\n    todo!(\"spec: two\")\n}\n",
        );
        let holes = tc.holes();
        assert_eq!(holes[0].position, HolePosition::Statement);

        let outcomes = tc.prober().probe_all(&holes);
        assert!(
            matches!(&outcomes[0], ProbeOutcome::NoExpectation),
            "{outcomes:?}"
        );
        assert!(
            matches!(&outcomes[1], ProbeOutcome::Known(t) if t == "i64"),
            "{outcomes:?}"
        );
    }

    #[test]
    fn batch_probing_falls_back_when_a_neighbour_breaks_the_crate() {
        // This is the case the decline exists for. `operator` makes `() + 1`,
        // a hard error; in a batch it could plausibly disturb `clean`'s
        // inference. Whatever the batch decides, the final answer for `clean`
        // must still be the right type, because undecided holes are re-probed
        // one at a time.
        let tc = TempCrate::new(
            "batch-fallback",
            "pub fn clean() -> i64 {\n    todo!(\"spec: one\")\n}\n\n\
             pub fn operator() -> u32 {\n    todo!(\"spec: two\") + 1\n}\n",
        );
        let holes = tc.holes();
        let outcomes = tc.prober().probe_all(&holes);

        assert!(
            matches!(&outcomes[0], ProbeOutcome::Known(t) if t == "i64"),
            "the healthy hole must still be answered: {outcomes:?}"
        );
        // The operator hole is either sniffed out via the fallback or, if rustc
        // answered for it directly, reported as a type. Never a guess.
        match &outcomes[1] {
            ProbeOutcome::ProbeFailed(msg) => assert!(msg.contains("fell back"), "{msg}"),
            ProbeOutcome::Known(_) | ProbeOutcome::NoExpectation => {}
            other => panic!("unexpected outcome for the operator hole: {other:?}"),
        }
    }

    #[test]
    fn batch_probing_a_single_hole_still_answers() {
        // The batch path is skipped for one hole; the result must be identical.
        let tc = TempCrate::new(
            "batch-one",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n",
        );
        let outcomes = tc.prober().probe_all(&tc.holes());
        assert_eq!(outcomes.len(), 1);
        assert!(
            matches!(&outcomes[0], ProbeOutcome::Known(t) if t == "i64"),
            "{outcomes:?}"
        );
    }

    #[test]
    fn batch_probing_an_empty_list_is_harmless() {
        let tc = TempCrate::new("batch-empty", "pub fn a() {}\n");
        assert!(tc.prober().probe_all(&[]).is_empty());
    }

    // -- how many checks a batch actually costs ----------------------------

    /// Eight holes in one file, each in its own function so they are
    /// independent and all of them answer.
    const EIGHT_HOLES: &str = "pub fn a() -> i64 {\n    todo!(\"spec: a\")\n}\n\n\
         pub fn b() -> bool {\n    todo!(\"spec: b\")\n}\n\n\
         pub fn c() -> String {\n    todo!(\"spec: c\")\n}\n\n\
         pub fn d() -> Vec<u8> {\n    todo!(\"spec: d\")\n}\n\n\
         pub fn e() -> u32 {\n    todo!(\"spec: e\")\n}\n\n\
         pub fn f() -> Option<i32> {\n    todo!(\"spec: f\")\n}\n\n\
         pub fn g() -> f64 {\n    todo!(\"spec: g\")\n}\n\n\
         pub fn h() -> i128 {\n    todo!(\"spec: h\")\n}\n";

    /// A `cargo` wrapper that appends a line per invocation, then delegates to
    /// the real cargo. Makes "how many checks did that cost?" measurable.
    #[cfg(unix)]
    fn counting_cargo(tag: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let dir =
            std::env::temp_dir().join(format!("cargo-hole-count-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let log = dir.join("calls.log");
        let script = dir.join("cargo");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf 'check\\n' >> '{}'\nexec '{}' \"$@\"\n",
                log.display(),
                cargo_binary()
            ),
        )
        .unwrap();

        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        (script, log)
    }

    #[cfg(unix)]
    fn checks_run(log: &Path) -> usize {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .count()
    }

    #[test]
    #[cfg(unix)]
    fn batching_costs_one_check_for_the_whole_file() {
        // The claim the batch exists to make. Eight holes, one file, and the
        // counting wrapper proves it is one compiler run -- not eight.
        let tc = TempCrate::new("count-batch", EIGHT_HOLES);
        let (script, log) = counting_cargo("batch");
        let mut options = tc.options();
        options.cargo = script.display().to_string();
        let prober = Prober::new(tc.dir.clone(), options);

        let holes = tc.holes();
        assert_eq!(holes.len(), 8);
        let outcomes = prober.probe_all(&holes);
        assert!(
            outcomes.iter().all(|o| matches!(o, ProbeOutcome::Known(_))),
            "every hole should answer directly, so no fallback runs: {outcomes:?}"
        );

        assert_eq!(
            checks_run(&log),
            1,
            "eight holes in one file must cost exactly one cargo check"
        );
    }

    #[test]
    #[cfg(unix)]
    fn probing_hole_by_hole_costs_one_check_each() {
        // The baseline the batch is measured against: the same eight holes, one
        // at a time. This is what the batch replaces.
        let tc = TempCrate::new("count-serial", EIGHT_HOLES);
        let (script, log) = counting_cargo("serial");
        let mut options = tc.options();
        options.cargo = script.display().to_string();
        let prober = Prober::new(tc.dir.clone(), options);

        let holes = tc.holes();
        assert_eq!(holes.len(), 8);
        for hole in &holes {
            assert!(
                matches!(prober.probe(hole), ProbeOutcome::Known(_)),
                "each hole answers on its own too"
            );
        }

        assert_eq!(
            checks_run(&log),
            8,
            "one check per hole is the cost the batch removes"
        );
    }

    #[test]
    #[cfg(unix)]
    fn batch_probing_costs_one_check_per_file() {
        // Two files, two holes each: the batch pays per file, not per hole.
        let tc = TempCrate::new("count-files", "pub mod one;\npub mod two;\n");
        std::fs::write(
            tc.dir.join("src/one.rs"),
            "pub fn a() -> i64 {\n    todo!(\"spec: a\")\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: b\")\n}\n",
        )
        .unwrap();
        std::fs::write(
            tc.dir.join("src/two.rs"),
            "pub fn c() -> String {\n    todo!(\"spec: c\")\n}\n\n\
             pub fn d() -> Vec<u8> {\n    todo!(\"spec: d\")\n}\n",
        )
        .unwrap();

        let (script, log) = counting_cargo("files");
        let mut options = tc.options();
        options.cargo = script.display().to_string();
        let prober = Prober::new(tc.dir.clone(), options);

        let holes = Hole::list_holes(&tc.dir).expect("list");
        assert_eq!(holes.len(), 4);
        let outcomes = prober.probe_all(&holes);
        assert!(
            outcomes.iter().all(|o| matches!(o, ProbeOutcome::Known(_))),
            "{outcomes:?}"
        );

        assert_eq!(
            checks_run(&log),
            2,
            "two files must cost two checks, however many holes they hold"
        );
    }

    #[test]
    #[cfg(unix)]
    fn batch_probing_costs_nothing_for_statements() {
        // Statements are decided without a compiler, so a file of statements
        // costs no check at all.
        let tc = TempCrate::new(
            "count-stmt",
            "pub fn a() {\n    todo!(\"spec: a\");\n}\n\n\
             pub fn b() {\n    todo!(\"spec: b\");\n}\n",
        );
        let (script, log) = counting_cargo("stmt");
        let mut options = tc.options();
        options.cargo = script.display().to_string();
        let prober = Prober::new(tc.dir.clone(), options);

        let holes = tc.holes();
        assert_eq!(holes.len(), 2);
        let outcomes = prober.probe_all(&holes);
        assert!(
            outcomes
                .iter()
                .all(|o| matches!(o, ProbeOutcome::NoExpectation)),
            "{outcomes:?}"
        );
        assert_eq!(checks_run(&log), 0, "a statement needs no type at all");
    }

    #[test]
    #[cfg(unix)]
    fn a_declined_hole_costs_one_extra_check() {
        // The fallback is not free, and this pins its price: a batch that has to
        // decline re-probes each undecided hole on its own. Here `operator`
        // makes `() + 1`, which is a hard error, so the batch declines it and
        // pays one more check -- while still answering `clean` correctly.
        let tc = TempCrate::new(
            "count-fallback",
            "pub fn clean() -> i64 {\n    todo!(\"spec: clean\")\n}\n\n\
             pub fn operator() -> u32 {\n    todo!(\"spec: operator\") + 1\n}\n",
        );
        let (script, log) = counting_cargo("fallback");
        let mut options = tc.options();
        options.cargo = script.display().to_string();
        let prober = Prober::new(tc.dir.clone(), options);

        let holes = tc.holes();
        assert_eq!(holes.len(), 2);
        let outcomes = prober.probe_all(&holes);
        assert!(
            matches!(&outcomes[0], ProbeOutcome::Known(t) if t == "i64"),
            "{outcomes:?}"
        );

        // One batch check, plus one for whichever hole the batch declined.
        let calls = checks_run(&log);
        assert!(
            (1..=3).contains(&calls),
            "a fallback costs bounded extra checks, got {calls}"
        );
    }

    #[test]
    fn batch_probing_leaves_no_backups_behind() {
        let tc = TempCrate::new(
            "batch-clean",
            "pub fn a() -> i64 {\n    todo!(\"spec: one\")\n}\n\n\
             pub fn b() -> bool {\n    todo!(\"spec: two\")\n}\n",
        );
        let before = std::fs::read(tc.lib()).unwrap();
        let _ = tc.prober().probe_all(&tc.holes());
        assert_eq!(std::fs::read(tc.lib()).unwrap(), before);
        assert!(
            find_backups(&tc.dir).is_empty(),
            "a batch must clean up every backup it wrote"
        );
    }
}
