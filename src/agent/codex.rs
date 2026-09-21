use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

use crate::{agent::ModelConfig, util};

pub const CODEX_BIN: &str = "codex";

pub fn handle(model: &ModelConfig, prompt: &str) -> Result<String> {
    // The prompt goes to *stdin*, not argv. `-` means "read the prompt from
    // stdin", so passing the prompt as a trailing argument leaves the agent
    // reading an empty prompt -- it still answers, which makes the bug silent.
    // A spec plus its enclosing function is also far too long for a comfortable
    // command line, and would risk the system `ARG_MAX` limit.
    let mut cmd = Command::new(CODEX_BIN);
    cmd.args([
        "exec",
        // Structured events on stdout, so the answer is picked out by parsing
        // rather than by guessing at human-readable output.
        "--json",
        // We want text back, never file edits: `codex` otherwise writes files
        // in its working directory, which would fight with cargo-hole's own
        // patch/verify cycle.
        "--sandbox",
        "read-only",
        // The crate under repair is not necessarily a git repo.
        "--skip-git-repo-check",
        "--color",
        "never",
    ]);
    // An empty name defers to the CLI's own configured model.
    if !model.model_name.trim().is_empty() {
        cmd.args(["--model", &model.model_name]);
    }
    cmd.arg("-");
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot run `{CODEX_BIN}` -- is it installed and on PATH?"))?;
    child
        .stdin
        .take()
        .expect("stdin was piped above")
        .write_all(prompt.as_bytes())?;

    let output = child.wait_with_output()?;
    
    let stdout = output.stdout;
    let stdout = std::str::from_utf8(&stdout)?;
    let events = parse_codex_jsonl(&stdout);

    if !output.status.success() {
        // The agent's own explanation is far more useful than "exit 1".
        let detail = events.diagnostics();
        if detail.is_empty() {
            anyhow::bail!("`{CODEX_BIN} exec` exited with {}", output.status);
        }
        anyhow::bail!(
            "`{CODEX_BIN} exec` exited with {}: {detail}",
            output.status
        );
    }

    // Warnings are common and benign: `codex` reports an unknown model as an
    // `error` item while still exiting 0 and answering correctly. Reporting
    // them on stderr keeps them visible without failing the call.
    for warning in &events.warnings {
        eprintln!("warning: {CODEX_BIN}: {}", warning);
    }

    if let Some(message) = events.message {
        return Ok(message);
    }

    // No structured event at all: fall back to raw stdout. The reply is still
    // held to the "one expression" shape by the caller (`extract_expression`),
    // so this cannot smuggle a bad answer through -- it only avoids losing a
    // usable reply if the event format ever changes.
    if !events.parsed_any && !stdout.trim().is_empty() {
        return Ok(stdout.trim().to_string());
    }

    let detail = events.diagnostics();
    if detail.is_empty() {
        anyhow::bail!("`{CODEX_BIN} exec` produced no reply");
    }
    anyhow::bail!("`{CODEX_BIN} exec` produced no reply: {detail}")
}



/// What a `codex exec --json` run told us.
#[derive(Debug, Default, PartialEq)]
struct CodexEvents {
    /// Text of the last completed `agent_message`, i.e. the reply.
    message: Option<String>,
    /// Non-fatal diagnostics worth showing, including failures.
    warnings: Vec<String>,
    /// True when at least one line parsed as a JSON event.
    parsed_any: bool,
}

impl CodexEvents {
    /// A single line describing what we learned, for error messages.
    fn diagnostics(&self) -> String {
        if self.warnings.is_empty() {
            return String::new();
        }
        let joined = self
            .warnings
            .iter()
            .map(|w| util::truncate(w, 200))
            .collect::<Vec<_>>()
            .join("; ");
        util::truncate(&joined, 800)
    }
}

/// Parse the JSONL event stream that `codex exec --json` writes to stdout.
///
/// Lines that are not valid JSON are skipped rather than fatal: the CLI mixes
/// log output into stdout, and a single stray line should not discard a good
/// answer.
///
/// The *last* `agent_message` wins. Only `item.completed` is trusted for the
/// reply text; `item.updated` carries streaming deltas, and reading a partial
/// delta as the final answer would be worse than reading nothing.
fn parse_codex_jsonl(stdout: &str) -> CodexEvents {
    let mut events = CodexEvents::default();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        events.parsed_any = true;

        match value
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
        {
            "item.completed" => {
                let Some(item) = value.get("item") else {
                    continue;
                };
                match item.get("type").and_then(|v| v.as_str()) {
                    Some("agent_message") => {
                        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                            events.message = Some(text.to_string());
                        }
                    }
                    // An `error` item is not necessarily fatal -- see `complete`.
                    Some("error") => {
                        if let Some(message) = item.get("message").and_then(|v| v.as_str()) {
                            events.warnings.push(message.to_string());
                        }
                    }
                    _ => {}
                }
            }
            // Terminal failures. Shapes vary, so accept the common spellings.
            "turn.failed" | "error" | "thread.failed" => {
                let message = value
                    .get("message")
                    .and_then(|v| v.as_str())
                    .or_else(|| value.pointer("/error/message").and_then(|v| v.as_str()))
                    .unwrap_or("the agent reported a failure");
                events.warnings.push(message.to_string());
            }
            _ => {}
        }
    }

    events
}
