# Providers: implementing `Agent::BuiltIn`

Design note. **No code has been changed** — this is the plan only.

## 1. Where things stand

`Agent::BuiltIn` is declared, handled in three `match` arms, and **unreachable**:

| Fact | Evidence |
|---|---|
| Nothing constructs it in a real run | `Agent::from_args` — the only path `fill` takes — always returns `Agent::Cli(...)` |
| `impl Default for Agent` builds it | `src/agent.rs:384`, but `Agent::default()` is **never called** |
| No flag selects it | `fill --help` has no `--agent`; `Agent::handle`'s error tells users to pass one anyway |

Four `ModelConfig` fields are **dead** — parsed, documented, never read:

```rust
// src/agent/codex.rs — the entire use of `model`:
if !model.model_name.trim().is_empty() {
    cmd.args(["--model", &model.model_name]);
}
```

`codex::handle` reads `model_name` only. `Filler` reads `max_attempts` only.
So **`base_url`, `api_key`, `max_tokens`, `temperature` are read by nobody.**

That dead config is the tell: those four fields only mean anything to a
client that speaks HTTP itself. `BuiltIn` was always intended to be a direct
OpenAI-compatible client — it was just never written. `README.md` says as much:

> **Provider pluggability.** `docs/providers.md`, a `Provider` trait, a `script`
> provider and the `[model] provider/command/args` keys do not exist — the only
> agent is the `codex` CLI.

### Bug to fix regardless

`src/agent.rs:363` names a flag that does not exist:

```
$ cargo hole fill --agent cli
error: unexpected argument '--agent' found
```

## 2. The measurement that shapes the design

From the A/B experiment (real `codex`, 15 holes, isolated vs. file-present):

| | `codex` (agent-shaped) | `BuiltIn` (chat-shaped) |
|---|---|---|
| Can read `src/lib.rs` itself | **Yes** | **No** |
| Value of the `Expected type:` line | mostly redundant | **the only source of type info** |
| Isolated hit rate | — | no-probe 4/6 → **probe 5/6** |

The decisive case was `retries() -> u16` with spec "The number of retries":
without probe the model returned **nothing** (`usize`/`i32`/`u16` are all
plausible from the words); with probe it returned `3`.

Two consequences for `BuiltIn`:

1. **Probe stops being optional here.** In `codex` runs it is a tie-breaker; for
   a chat model it is the only type signal in the prompt.
2. **`fn_sig` should be in the prompt.** It is already on `Hole`
   (`hash_key` uses it), and it is *not* currently sent:

```
File: src/lib.rs
Line: 7
Position: expression
[Expected type: f64]      <- only type carrier today

Specification:
The fee charged on this amount
```

A chat model sees no signature and no surrounding source. Adding `fn: ...`
helps every provider and is a one-line change.

> Safety property worth keeping: `ProbeFailed`/`Broken` emit **no** type line,
> so those prompts are byte-identical to `--no-probe`. Probe can never make a
> prompt worse.

## 3. Transport options, measured

### LLM-specific crates

Queried the crates.io sparse index for **required, non-optional** deps:

| crate | ver | required deps | runtime |
|---|---|---|---|
| `async-openai` | 0.42.0 | **3** | HTTP behind optional features |
| `chatgpt` | 0.2.0 | 2 | sync |
| `kalosm` | 0.4.0 | 3 | sync |
| `ollama-rs` | 0.3.6 | 9 | reqwest |
| `openai-api-rs` | 10.0.1 | 8 | reqwest + tokio |
| `llm` | 1.3.8 | 19 | tokio + reqwest + async-trait |
| `genai` | 0.7.0-beta.23 | 19 | tokio + reqwest + futures |
| `rig-core` | 0.42.0 | 25 | tokio + reqwest + eventsource-stream |
| `llm-chain` | 0.13.0 | 18 | tokio + reqwest |
| `mistralrs` | 0.8.1 | 20 | tokio + reqwest |
| `anthropic` | 0.0.8 | 14 | reqwest + tokio |

**`async-openai` is the only serious candidate**, and it is a genuinely
interesting hybrid: with `default-features = false` it pulls just
`getrandom/serde/serde_json`, and the HTTP stack sits behind `_api`
(`dep:reqwest`, `dep:tokio`, `dep:tower`, ...). So one can take its **types**
(`chat-completion-types`) without the runtime, and bring one's own transport.

Everything else either drags in tokio + reqwest (reqwest *requires* tokio —
its `blocking` feature expands to `tokio/sync`, and tokio is a non-optional
dependency) or is a thin, low-traffic wrapper.

**Caveat:** I could not verify the types-only tree end-to-end — resolving
`async-openai` needs a crate download, and the escalation for that was
declined. Treat the "3 required deps" figure as index-derived, not
build-verified.

### The three real choices

| | new deps | verified here | notes |
|---|---|---|---|
| **A. `curl` subprocess** | **0** | **yes, end-to-end** | reuses `prober`'s `run_command`/`kill_tree` |
| B. `async-openai` types + own transport | 3 (+ curl) | partially | nice types, still needs a transport |
| C. `reqwest` (blocking) | ~40 | no | pulls hyper + tokio + rustls |

Measured facts for **A** (local OpenAI-compatible server, curls below):

```
body via stdin (-d @-)      -> http=200, prompt reached server   # no ARG_MAX limit
key via -H                  -> 2 processes expose it in `ps`      # leaks
key via -K configfile       -> server saw the header, key not in argv
--fail-with-body on 401     -> non-zero exit AND the error body
unreachable port            -> curl: (7) ...
```

`-d @-` matters for the same reason `codex.rs` already documents:

> The prompt goes to *stdin*, not argv. ... would risk the system `ARG_MAX` limit.

So A requires: body on stdin, key via a `0600` temp curlrc deleted after use,
`--fail-with-body`, and `--max-time`. It reuses `kill_tree` — curl spawns TLS
threads, and killing only the direct child leaves the pipe open (the exact bug
`prober.rs` already fixed for `cargo`).

**Recommendation: A**, with B's *types* deferred. `cargo-hole` already depends
on external binaries (`cargo`, `rustc`, `codex`); one more does not change its
character, and it keeps `Cargo.toml` free of network deps — consistent with the
suite being hermetic.

## 4. Proposed shape

Three steps; each is independently verifiable and step 1 changes no behaviour.

### Step 1 — `Provider` trait (pure refactor)

Generalise the existing `Cli` enum; `codex` becomes one impl. No behaviour
change, all 175 tests must stay green.

```rust
pub trait Provider {
    fn handle(&self, model: &ModelConfig, prompt: &str) -> Result<String>;
    fn label(&self) -> String;
}
```

This is where `max_tokens` / `temperature` finally get a reader.

### Step 2 — `script` provider (test double)

```toml
[model]
provider = "script"
command  = "./tests/fake-model.sh"
```

Prompt on stdin, reply on stdout. This is the keystone: `README` commits to a
hermetic suite —

> The suite is hermetic: ... need no network, and there is no ignored/live-provider test group.

— and a `script` provider is what makes step 3 testable offline. It is also
exactly the harness I hand-rolled for the A/B runs, productised.

### Step 3 — `builtin` provider (HTTP via curl)

```rust
// body: serde_json -> stdin, never argv
let body = json!({
    "model": model.model_name,
    "messages": [
        {"role": "system", "content": SYSTEM_PROMPT},
        {"role": "user",   "content": prompt},
    ],
    "max_tokens":  model.max_tokens,   // finally read
    "temperature": model.temperature,  // finally read
});
```

Config key `[model] provider = "cli" | "script" | "builtin"`. Note that
`provider` is currently **rejected** by `deny_unknown_fields`:

```
2 | provider = "cli"
  | ^^^^^^^^ unknown field `provider`, expected one of `model`, `base_url`, `api_key`, `api_key_env`
```

Adding it is a deliberate, documented schema change.

## 5. Order of work

| # | Change | Deps | Verifiable by |
|---|---|---|---|
| 1 | Fix the fake `--agent` message in `Agent::handle` | none | reading it |
| 2 | Add `fn_sig` to `build_prompt` | none | prompt-dump test |
| 3 | `Provider` trait + `codex` impl | none | 175 tests stay green |
| 4 | `script` provider | none | new offline tests |
| 5 | `builtin` HTTP provider | none (curl) | `script`-style stub server |
| 6 | `[model] provider` key + `--provider` flag | none | config tests |

Doing 3–4 before 5 is what keeps 5 testable without network.

## 6. Open questions

1. **Transport** — A (curl, recommended), B, or C.
2. **`fn_sig` in the prompt** — helps every provider; changes prompts. The
   ledger keys on spec/sig/position, not the prompt, so no cache is
   invalidated, but it is worth doing once.
3. **Scope** — steps 1–2 only, or all six.
4. **`async-openai` types** — worth taking just the type definitions, given it
   still needs a transport either way? My read: no, for a request this small
   (`model`, `messages`, `max_tokens`, `temperature`) — but it is the one
   genuinely defensible library option.

## 7. Related gaps found while investigating

- `--timeout` is parsed by `fill` (`src/cmdline.rs:66`) and reaches neither
  `codex::handle` (which uses a bare `wait_with_output()`, so **a hung model
  hangs `fill` forever**) nor `Prober` (which uses its own 300s default).
- `src/verifier.rs` is empty; `fill` reserves a slot for the compile gate.
- `--at <file>:<line>` is accepted and ignored, per README.