use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::hole::HoleIndex;
use crate::prober::{ProbeOutcome, Prober};
use crate::{agent::Agent, hole::Hole, util::display_rel_path};

pub const OPEN_MARKER: &str = "<<<HOLE>>>";
pub const CLOSE_MARKER: &str = "<<<END>>>";

/// How many compiler errors to quote back when asking again for a hole.
///
/// Capped because one bad answer can produce a long cascade, and the first few
/// errors already say what is wrong. The rest would only crowd the prompt.
pub const MAX_REPORTED_ERRORS: usize = 8;

/// What the probe learned, keyed by hole so `fill` can look it up later.
///
/// A `HashMap` rather than a `Vec` because `fill` reaches a hole's verdict by
/// identity, and the batch and the per-hole fallback both key off the same
/// value.
pub type Expectations = HashMap<HoleIndex, ProbeOutcome>;

/// Fills holes by asking an [`Agent`] for one expression per hole.
///
/// The prompt carries the spec, the hole's position, and — when the type probe
/// can find one — the type rustc expects at that position. The expected type is
/// the difference between asking a model to invent a value and asking it to
/// produce one of a known type.
pub struct Filler<'a> {
    root: PathBuf,
    agent: &'a Agent,
    /// `None` disables probing (`--no-probe`), so the prompt carries no expected
    /// type and every hole costs no `cargo check`.
    prober: Option<Prober>,
}

impl<'a> Filler<'a> {
    /// A filler that probes for expected types.
    pub fn new(root: PathBuf, agent: &'a Agent) -> Filler<'a> {
        let prober = Prober::new(root.clone(), crate::prober::ProberOptions::from_env());
        Filler {
            root,
            agent,
            prober: Some(prober),
        }
    }

    /// A filler that never probes, for `--no-probe`.
    pub fn without_probe(root: PathBuf, agent: &'a Agent) -> Filler<'a> {
        Filler {
            root,
            agent,
            prober: None,
        }
    }

    /// Probe every hole up front, so the whole run costs one `cargo check` per
    /// file instead of one per hole.
    ///
    /// Call this before filling. [`Filler::fill_hole_with`] then reads the
    /// verdict instead of probing again. Without it, each `fill_hole_with` falls
    /// back to probing its own hole, which is correct but slower.
    ///
    /// Probing mutates and restores each file byte-for-byte, so this is safe to
    /// call before any file has been read or written.
    pub fn probe_all(&self, holes: &[Hole]) -> Expectations {
        let Some(prober) = self.prober.as_ref() else {
            return Expectations::new();
        };

        let outcomes = prober.probe_all(holes);
        let mut out = Expectations::with_capacity(holes.len());
        for (hole, outcome) in holes.iter().zip(outcomes) {
            if let ProbeOutcome::Broken(e) = &outcome {
                eprintln!(
                    "warning: could not probe {}: {e:#}; filling from the spec alone",
                    hole.location()
                );
            }
            out.insert(hole.index_key(), outcome);
        }
        out
    }

    /// Ask the agent for an expression to put in `hole`.
    ///
    /// Returns that expression together with the number of model calls it took.
    /// The expression is returned bare rather than already spliced into the
    /// file: the caller owns the file contents, and it needs the expression on
    /// its own to record in the ledger.
    ///
    /// Pinned and unresolvable holes are refused rather than filled: the caller
    /// is expected to have filtered them out already, so reaching either check
    /// is a bug worth reporting instead of patching bytes that were never
    /// validated.
    pub fn fill_hole(&self, hole: &Hole) -> Result<(String, u32)> {
        self.fill_hole_with(hole, None)
    }

    /// Ask again for a hole whose previous answer failed the compile gate.
    ///
    /// `errors` are the rustc messages the gate blamed on this hole. They go
    /// into the prompt as feedback, which is the whole point: a bare retry would
    /// ask the same question with the same information and tend to get the same
    /// answer back.
    ///
    /// A fresh probe is deliberately *not* run. The expected type is a property
    /// of the hole and its neighbours, and the neighbours have not changed in a
    /// way the probe could see -- it patches and restores, so it never observes
    /// the filled tree. Reusing the caller's expectation keeps the retry cheap.
    pub fn refill_hole(
        &self,
        hole: &Hole,
        known: Option<&Expectations>,
        errors: &[String],
    ) -> Result<(String, u32)> {
        let expected: Option<&ProbeOutcome> = known.and_then(|k| k.get(&hole.index_key()));

        let mut feedback = String::from(
            "Your previous answer compiled with errors. Fix it. The compiler reported:\n",
        );
        for e in errors.iter().take(MAX_REPORTED_ERRORS) {
            feedback.push_str("  - ");
            feedback.push_str(e);
            feedback.push('\n');
        }
        if errors.len() > MAX_REPORTED_ERRORS {
            feedback.push_str(&format!(
                "  (and {} more error(s))\n",
                errors.len() - MAX_REPORTED_ERRORS
            ));
        }

        // One round, using the same reply-parsing and retry-on-provider-failure
        // path as the first attempt, with the compiler's complaint as the seed
        // feedback.
        self.ask(hole, expected, Some(&feedback))
    }

    /// As [`Filler::fill_hole`], but reusing a verdict from
    /// [`Filler::probe_all`] when one was supplied for this hole.
    ///
    /// The probe runs at most once per hole, before the attempt loop: it costs a
    /// `cargo check`, and the expected type cannot change between attempts
    /// because nothing is written until one succeeds.
    ///
    /// Probing patches the hole's file on disk and restores it byte-for-byte, so
    /// any `src` the caller is holding stays valid. That property is what makes
    /// it safe to probe while `fill` is midway through a file.
    pub fn fill_hole_with(
        &self,
        hole: &Hole,
        known: Option<&Expectations>,
    ) -> Result<(String, u32)> {
        if hole.pinned {
            bail!("{} is pinned, so it is never regenerated", hole.location());
        }
        if let Some(reason) = &hole.unresolvable {
            bail!(
                "the byte range of {} could not be validated ({reason}), so it is never \
                 patched",
                hole.location()
            );
        }

        // Reuse the batch's verdict when there is one; otherwise probe just this
        // hole, which is the only option if `probe_all` was never called.
        // `ProbeOutcome` holds an `anyhow::Error` and so cannot be cloned, hence
        // the borrow: `fallback` keeps the locally-probed result alive.
        let fallback;
        let expected: Option<&ProbeOutcome> = match known.and_then(|k| k.get(&hole.index_key())) {
            Some(outcome) => Some(outcome),
            None => {
                fallback = self.probe(hole);
                fallback.as_ref()
            }
        };

        self.ask(hole, expected, None)
    }

    /// Run the attempt loop: ask, parse, retry with feedback until the reply is
    /// usable or the attempts run out.
    ///
    /// `seed` is feedback to include in the *first* prompt, which is how a
    /// retry after a failed compile gate carries the compiler's complaint.
    fn ask(
        &self,
        hole: &Hole,
        expected: Option<&ProbeOutcome>,
        seed: Option<&str>,
    ) -> Result<(String, u32)> {
        let mut feedback: Option<String> = seed.map(str::to_string);
        let max_attempts = self.agent.max_attempts();

        for attempt in 1..=max_attempts {
            let prompt = self.build_prompt(hole, expected, feedback.as_deref());
            let raw = match self.agent.handle(&prompt) {
                Ok(raw) => raw,
                Err(e) => {
                    // Worth retrying: a provider failure is often transient
                    // (a rate limit, a restarting daemon), and `src` is
                    // untouched until an attempt succeeds, so a retry cannot
                    // corrupt the file.
                    feedback = Some(format!(
                        "Your previous reply could not be used: {e}. Reply with exactly one \
                         expression between {OPEN_MARKER} and {CLOSE_MARKER}."
                    ));
                    continue;
                }
            };

            let code = match extract_expression(&raw) {
                Ok(code) => code,
                Err(e) => {
                    feedback = Some(format!(
                        "Your previous reply was rejected: {e} Reply with exactly one \
                         expression between {OPEN_MARKER} and {CLOSE_MARKER}, and nothing else."
                    ));
                    continue;
                }
            };

            return Ok((code, attempt));
        }

        bail!(
            "could not fill {} after {max_attempts} attempt(s)",
            hole.location()
        )
    }

    /// Probe for the hole's expected type, or `None` when probing is off.
    ///
    /// A broken probe is a warning, not a failure: the model can still work from
    /// the spec alone, which is what the tool did before the probe existed.
    /// Losing the whole fill over a missing `cargo` would be a poor trade.
    fn probe(&self, hole: &Hole) -> Option<ProbeOutcome> {
        let prober = self.prober.as_ref()?;
        let outcome = prober.probe(hole);
        if let ProbeOutcome::Broken(e) = &outcome {
            eprintln!(
                "warning: could not probe {}: {e:#}; filling from the spec alone",
                hole.location()
            );
        }
        Some(outcome)
    }

    pub fn build_prompt(
        &self,
        hole: &Hole,
        expected: Option<&ProbeOutcome>,
        feedback: Option<&str>,
    ) -> String {
        let mut prompt = String::new();

        prompt.push_str(&format!(
            "File: {}\n",
            display_rel_path(&self.root, &hole.file)
        ));
        prompt.push_str(&format!("Line: {}\n", hole.line));
        prompt.push_str(&format!("Position: {}\n", hole.position.as_str()));

        if let Some(expectation) = expected_type_hint(expected) {
            prompt.push_str(&format!("Expected type: {expectation}\n"));
        }

        prompt.push_str(&format!("\nSpecification:\n{}\n", hole.spec));

        prompt.push_str(&format!(
            "\nImplement just this one hole. Reply with one expression between \
             {OPEN_MARKER} and {CLOSE_MARKER}."
        ));

        if let Some(fb) = feedback {
            prompt.push_str(&format!("\n\n{fb}"));
        }

        prompt
    }
}

/// Render the probe's verdict as a line for the prompt.
///
/// `None` means no line is emitted at all, which is the right answer for
/// `--no-probe` and for a probe that could not run: inventing "unknown" as an
/// expected type would only invite the model to guess at a type that was never
/// established.
fn expected_type_hint(expected: Option<&ProbeOutcome>) -> Option<String> {
    match expected? {
        ProbeOutcome::Known(ty) => Some(ty.clone()),
        // Said plainly rather than omitted, so the model does not assume a type
        // was checked and found to be something obvious.
        ProbeOutcome::NoExpectation => {
            Some("none discovered -- infer it from the specification".to_string())
        }
        ProbeOutcome::ProbeFailed(_) | ProbeOutcome::Broken(_) => None,
    }
}

/// Pull the expression out of a model reply.
///
/// Prefers the explicit markers the prompt asks for, then falls back to the
/// whole reply: models routinely wrap an otherwise-correct answer in prose or a
/// code fence, and rejecting it over formatting would be unhelpful. Splicing the
/// raw reply instead would put the markers themselves into the source file.
fn extract_expression(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("the model returned an empty reply.");
    }

    let body = match (trimmed.find(OPEN_MARKER), trimmed.find(CLOSE_MARKER)) {
        (Some(open), Some(close)) if close > open => &trimmed[open + OPEN_MARKER.len()..close],
        _ => trimmed,
    };

    let cleaned = strip_fences(body).trim().to_string();
    if cleaned.is_empty() {
        bail!("the model's reply had no expression in it.");
    }
    Ok(cleaned)
}

/// Strip a ``` fence (with or without a language tag) around `body`.
fn strip_fences(body: &str) -> String {
    let trimmed = body.trim();
    let Some(rest) = trimmed.strip_prefix("```") else {
        return trimmed.to_string();
    };
    // Drop the language tag on the opening fence, if any.
    let rest = match rest.find('\n') {
        Some(nl) => &rest[nl + 1..],
        None => return trimmed.to_string(),
    };
    match rest.rfind("```") {
        Some(end) => rest[..end].trim().to_string(),
        None => rest.trim().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_are_stripped_from_a_reply() {
        let code = extract_expression("<<<HOLE>>> 40 + 2 <<<END>>>").unwrap();
        assert_eq!(code, "40 + 2");
    }

    #[test]
    fn a_fenced_reply_is_accepted() {
        let code = extract_expression("```rust\n40 + 2\n```").unwrap();
        assert_eq!(code, "40 + 2");
    }

    #[test]
    fn prose_without_markers_is_kept_verbatim() {
        let code = extract_expression("  42  ").unwrap();
        assert_eq!(code, "42");
    }

    #[test]
    fn an_empty_reply_is_rejected() {
        assert!(extract_expression("   ").is_err());
        assert!(extract_expression("<<<HOLE>>>  <<<END>>>").is_err());
    }

    #[test]
    fn a_known_type_is_carried_into_the_prompt() {
        let hint = expected_type_hint(Some(&ProbeOutcome::Known("Vec<String>".into())));
        assert_eq!(hint.as_deref(), Some("Vec<String>"));
    }

    #[test]
    fn no_expectation_is_stated_rather_than_omitted() {
        // Silence would read as "no type was needed"; saying so explicitly
        // stops the model from assuming a type was verified.
        let hint = expected_type_hint(Some(&ProbeOutcome::NoExpectation));
        let hint = hint.expect("a hint line is emitted");
        assert!(hint.contains("none discovered"), "{hint}");
        assert!(hint.contains("infer it from the specification"), "{hint}");
    }

    #[test]
    fn a_failed_probe_contributes_no_hint_at_all() {
        // A failure or a missing cargo must not become "unknown": that would
        // invite the model to guess a type nothing established.
        assert!(expected_type_hint(None).is_none());
        assert!(expected_type_hint(Some(&ProbeOutcome::ProbeFailed("boom".into()))).is_none());
        assert!(
            expected_type_hint(Some(&ProbeOutcome::Broken(anyhow::anyhow!("no cargo")))).is_none()
        );
    }
}
