# cargo-hole

Find `todo!()` placeholders that carry a written specification, and fill them in
with a model.

```rust
pub fn resize(img: &Image) -> Image {
    todo!("spec: scale to fit within 1024px on the longest side, keep the aspect
           ratio, never upscale; 1920x1080 -> 1024x576")
}
```

A hole is `todo!("spec: ...")` or `unimplemented!("spec: ...")`. The `spec:`
prefix is the explicit opt-in: a `todo!()` without it is not a hole.

## The idea: rustc as a type-query API

`todo!()`'s type is `!`, which coerces to anything, so it never produces a type
error on its own. Replace it with `()` and rustc answers with the type that was
expected there:

```
E0308: expected `Image`, found `()`
```

That yields the expected type, the exact span, and real inference results —
including coercions — for the price of one `cargo check`. **This probe is not
implemented yet:** `src/prober.rs` is empty and `fill` sends the model only the
spec text and the hole's syntactic position. `--no-probe` is accepted and
currently has no effect. See [Current status](#current-status).

## Commands

`cargo hole` is a cargo subcommand, so it is invoked as `cargo hole <command>`.

```bash
cargo hole list  --path .                 # every hole, with its spec and status
cargo hole list  --path . --width 40      # truncate specs more aggressively
cargo hole list  --path . --file lib.rs   # only holes in `lib.rs`
cargo hole list  --path . --fail-on-unelaborated   # exit 1 if any hole remains
cargo hole fill  --path .                 # fill holes, writing to .cargo-hole/
cargo hole fill  --path . --in-place      # ...and write back over the originals
```

Both commands take `--path`, the crate root to work on (default `.`). It is
canonicalised, and unreadable paths are a fatal error.

### `list`

```
$ cargo hole list --path .
4 hole(s) in /tmp/demo

[ 1] src/lib.rs:2  open  (todo, statement)
     fn:   fn stmt()
     spec: statement position

[ 2] src/lib.rs:6  open  (todo, expression)
     fn:   fn tail() -> u32
     spec: tail expression

[ 3] src/lib.rs:10  open  (todo, expression)
     fn:   fn arg() -> u32
     spec: argument position

[ 4] src/lib.rs:14  open  (todo, macro-body)
     fn:   fn macrobody() -> Vec<u32>
     spec: inside a macro body

summary: 4 open, 0 pinned, 0 unresolvable
```

Status is one of `open`, `PINNED`, or `UNRESOLVABLE`. Holes are listed one per
blank-line-separated block: location, status, macro name, and syntactic
position; then the enclosing signature and the spec.

`--file` matches the **file name** exactly, not a path substring — `--file
lib.rs` works, `--file src/lib.rs` matches nothing.

### `fill`

```
$ cargo hole fill --path .
filling 2 hole(s) in . using cli:codex (its own model)

2 filled, 0 not filled, 0 skipped (2 model call(s))
```

The header names the agent actually in use. Without `--in-place`, results are
written to `<root>/.cargo-hole/<relative path>` and the originals are left
untouched; with it, files are overwritten in place. Files are grouped so each is
read and written once, and holes within a file are filled from the bottom up so
earlier edits cannot shift later byte offsets.

A hole that cannot be filled is reported on stderr and does **not** abandon the
rest of the file:

```
warning: skipping /tmp/demo/src/lib.rs:3: /tmp/demo/src/lib.rs:3 is pinned, so it is never regenerated

1 filled, 1 not filled, 0 skipped (1 model call(s))
```

So `not filled` counts holes that were attempted and refused; `skipped` is
reserved but currently always 0.

## Configuration

Settings are layered, most specific last:

1. the built-in defaults,
2. `.cargo-hole.toml` in the crate root,
3. the `CARGO_HOLE_*` environment variables,
4. the command line flags.

```toml
# .cargo-hole.toml
[model]
model       = "qwen3.7-max"   # empty/absent defers to the CLI's own config
base_url    = "http://127.0.0.1:11434/v1"
api_key_env = "MY_API_KEY"    # preferred: keeps the file safe to commit
api_key     = "sk-literal"    # honoured, but api_key_env wins if both are set

[fill]
max_attempts = 3
max_tokens   = 2048
temperature  = 0.0
```

| environment variable | effect |
|---|---|
| `CARGO_HOLE_CLI` | agent CLI to drive (only `codex` is known) |
| `CARGO_HOLE_MODEL` | model name |
| `CARGO_HOLE_BASE_URL` | base URL |
| `CARGO_HOLE_API_KEY` | API key |
| `CARGO_HOLE_MAX_ATTEMPTS` | attempts per hole |
| `CARGO_HOLE_MAX_TOKENS` | upper bound on generated tokens |
| `CARGO_HOLE_TEMPERATURE` | sampling temperature |

An empty variable or an empty flag is ignored rather than applied, so
`CARGO_HOLE_MODEL=` cannot silently erase a model named in the config file. A
value that fails to parse is warned about and dropped.

The config file is read from the crate root given to `--path`, **not** from the
working directory, so one shell can point `--path` at several crates and pick up
each crate's own config. Keys are validated: an unknown key makes the whole file
be dropped rather than half-applied, and the warning names the accepted keys.

```toml
[model]
provider = "cli"                       # -> unknown field `provider`
```

```
warning: ignoring /tmp/demo/.cargo-hole.toml: TOML parse error at line 2, column 1
  |
2 | provider = "cli"
  | ^^^^^^^^
unknown field `provider`, expected one of `model`, `base_url`, `api_key`, `api_key_env`
```

## Marking and excluding holes

- `todo!("spec: ...")` / `unimplemented!("spec: ...")` — a hole. Only the
  parenthesised form counts; `todo! { "spec: ..." }` is ignored. The argument
  list must be a single string literal and an optional trailing comma; anything
  more elaborate is not treated as a spec.
- `// hole:pinned` anywhere in the run of whitespace and `//` comments directly
  above the hole — the hole is reported as `PINNED` and never filled. Only `//`
  lines count: `/* hole:pinned */` is ignored, as is a marker on the same line
  as the hole or after it.
- `#[hole::pin]` / `#[pin]` — works when written directly on the hole itself
  (`#[pin]` above a statement-position `todo!`), but **not on the enclosing
  function, method, `impl` or `trait`**, which the docs and `has_pin_attribute`
  both promise. The cause is a shadowed stack: the item visitors push onto
  `pin_stack`, but `visit_macro` records the hole from `pin_stack.last()`
  *before* `visit_stmt_macro`/`visit_expr_macro` push the macro's own (usually
  `false`) entry, so the enclosing item's value is never the one read. Until
  that is fixed, put the attribute on the `todo!` itself or use
  `// hole:pinned`.

`spec:` holes inside another macro's token body are found too, because macro
bodies are walked as raw tokens — `vec![todo!("spec: ...")]` is a hole.

## Position detection

Each hole records where it sits in the syntax tree, which is the hint a type
probe would use.

| shape | position |
|---|---|
| `todo!("spec: ...");` as a whole statement | `statement` |
| tail expression, argument, `if` condition, … | `expression` |
| inside another macro's tokens: `vec![todo!("spec: ...")]` | `macro-body` |
| item position | `item` |
| could not be determined | `unknown` |

A statement-position hole is known to be `()` even when rustc's inference cannot
say so — which is exactly the case the probe exists to cover.

## Current status

Implemented: hole discovery and listing, spec parsing, the `// hole:pinned`
marker, position detection, the `codex` agent, retry-on-failure, config and
environment layering, and writing results in place or to `.cargo-hole/`.

Not implemented yet, and therefore not relied upon by anything:

- **The type probe.** `src/prober.rs` is empty, so no expected type reaches the
  model and there is no `cargo hole type` command. `--no-probe` is inert.
- **The compile gate.** `src/verifier.rs` is empty. `fill` splices whatever the
  model returns; `--no-verify` is inert. A bad reply is written out verbatim.
- **`cargo hole restore`.** The subcommand exists and panics with
  `not implemented` (exit 101). There is no `.cargo-hole.bak`, no restore guard
  and no probe lock.
- **`--at <file>:<line>`** and **`--timeout <seconds>`.** Accepted by the CLI and
  ignored; `--at` fills every hole anyway.
- **Provider pluggability.** `docs/providers.md`, a `Provider` trait, a `script`
  provider and the `[model] provider/command/args` keys do not exist — the only
  agent is the `codex` CLI.
- **`Hole::hole_id`**, a blake3 hash over spec, signature, impl context,
  position and edition, is implemented but never called by anything.
- **`libc`** is declared for process-group kills that `--timeout` would need; no
  code uses it.

## Development

```bash
cargo test            # 14 tests, all offline
cargo build
```

The suite is hermetic: the 14 tests live in `src/agent.rs` (10) and
`src/filler.rs` (4), need no network, and there is no ignored/live-provider test
group.

### Layout

| module | responsibility |
|---|---|
| `hole.rs` | `syn`-based extraction of holes, spec parsing, pin markers, positions |
| `cmdline.rs` | the `clap` CLI: `list`, `fill`, `restore` |
| `filler.rs` | prompt assembly, reply extraction, splicing into the file |
| `agent.rs` | config file and `CARGO_HOLE_*` layering, agent selection |
| `agent/codex.rs` | driving `codex exec --json`, parsing its JSONL events |
| `util.rs` | file walk, byte-level scanner (delimiters, comments, strings), line/column |
| `prober.rs` | empty; the type probe belongs here |
| `verifier.rs` | empty; the compile gate belongs here |
| `main.rs`, `lib.rs` | thin entry point and module list |
