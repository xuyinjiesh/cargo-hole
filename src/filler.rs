use std::path::PathBuf;

use anyhow::{Result, bail};

use crate::{agent::Agent, hole::Hole, util::display_rel_path};

pub const OPEN_MARKER: &str = "<<<HOLE>>>";
pub const CLOSE_MARKER: &str = "<<<END>>>";

/// Fills holes by asking an [`Agent`] for one expression per hole.
///
/// The type probe is not wired in yet (`src/prober.rs` is empty), so the prompt
/// currently carries only the spec and the hole's position.
pub struct Filler<'a> {
    root: PathBuf,
    agent: &'a Agent,
}

impl<'a> Filler<'a> {
    pub fn new(root: PathBuf, agent: &'a Agent) -> Filler<'a> {
        Filler { root, agent }
    }

    /// Ask the agent for an expression to put in `hole`.
    ///
    /// Returns that expression together with the number of model calls it took.
    /// The expression is returned bare rather than already spliced into the
    /// file: the caller owns the file contents, and it needs the expression on
    /// its own to record in the ledger.
    ///
    /// Nothing is written here, and `src` is not consulted: the prompt carries
    /// the spec and the hole's position, and the type probe that would read the
    /// surrounding file is not implemented yet (`src/prober.rs` is empty).
    ///
    /// Pinned and unresolvable holes are refused rather than filled: the caller
    /// is expected to have filtered them out already, so reaching either check
    /// is a bug worth reporting instead of patching bytes that were never
    /// validated.
    pub fn fill_holes(&self, hole: &Hole) -> Result<(String, u32)> {
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

        let mut feedback: Option<String> = None;
        let max_attempts = self.agent.max_attempts();

        for attempt in 1..=max_attempts {
            let prompt = self.build_prompt(hole, feedback.as_deref());
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

    pub fn build_prompt(&self, hole: &Hole, feedback: Option<&str>) -> String {
        let mut prompt = String::new();

        prompt.push_str(&format!(
            "File: {}\n",
            display_rel_path(&self.root, &hole.file)
        ));
        prompt.push_str(&format!("Line: {}\n", hole.line));
        prompt.push_str(&format!("Position: {}\n", hole.position.as_str()));

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
}
