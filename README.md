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
including coercions — for the price of one `cargo check`.

The probe runs batched: every hole in a file is replaced with `()` at once and
one `cargo check` answers all of them, because each `E0308` carries its own span.
A verdict the batch cannot settle is re-probed alone, so the batched answer is
never weaker than the serial one. `--no-probe` turns the whole thing off, and the
model then gets only the spec text and the hole's syntactic position.

## Commands

`cargo hole` is a cargo subcommand, so it is invoked as `cargo hole <command>`.

```bash
cargo hole list  --path .                 # every hole, with its spec and status
cargo hole list  --path . --width 40      # truncate specs more aggressively
cargo hole list  --path . --pretty        # group by file, wrap specs, colour
cargo hole list  --path . --file lib.rs   # only holes in `lib.rs`
cargo hole list  --path . --fail-on-unelaborated   # exit 1 if any hole remains
cargo hole fill  --path .                 # fill holes, writing to .cargo-hole/
cargo hole fill  --path . --in-place      # ...and write back over the originals
cargo hole build --path .                 # build the generated tree, source untouched
cargo hole build --path . --release       # flags after the command go to cargo
cargo hole run   --path .                 # build and run the generated tree
cargo hole run   --path . -- a b          # arguments after `--` go to the program
```

Every command takes `--path`, the crate root to work on (default `.`). It is
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

`--pretty` switches to a layout for reading rather than grepping: holes are
grouped under their file, the spec is wrapped instead of truncated, and a header
tallies the run. The default layout is unchanged and is what scripts should
parse.

`--color <auto|always|never>` adds ANSI emphasis; the default colours a terminal
and stays plain when piped, and `NO_COLOR` is respected. Colour is never the only
signal — every distinction it draws is also spelled out in the text.

Note that statuses are counted independently, so a hole that is both pinned and
unresolvable is counted in both columns.

`--file` matches the **file name** exactly, not a path substring — `--file
lib.rs` works, `--file src/lib.rs` matches nothing.

### `fill`

```
$ cargo hole fill --path .
filling 2 hole(s) in . using cli:codex (its own model)

2 filled, 0 cached, 0 not filled, 0 skipped (2 model call(s))
```

The header names the agent actually in use. Without `--in-place`, results are
written to `<root>/.cargo-hole/patch/<relative path>` and the originals are left
untouched; with it, files are overwritten in place. Files are grouped so each is
read and written once. All of a file's answers are spliced in a single
left-to-right pass, which is also what records where each answer landed -- needed
to tell which hole a compile error belongs to.

A hole that cannot be filled is reported on stderr and does **not** abandon the
rest of the file:

```
warning: skipping /tmp/demo/src/lib.rs:3: /tmp/demo/src/lib.rs:3 is pinned, so it is never regenerated

1 filled, 0 cached, 1 not filled, 0 skipped (1 model call(s))
```

So `not filled` counts holes that were attempted and refused; `cached` counts
holes answered from the ledger below; `skipped` counts pinned and unresolvable
holes, which are never attempted.

### The compile gate

Before anything is recorded, the generated tree is put through a compile gate.
The subtlety is that a plain `cargo check` of the crate root would never fail:
a hole is `todo!()`, whose type is `!` and coerces to anything, so a crate full
of unelaborated holes compiles cleanly — and without `--in-place` the real source
is never modified, so checking the root checks the *unfilled* tree. The gate
therefore patches every generated file into place, runs one `cargo check`, and
restores every file byte-for-byte.

If the tree does not compile, nothing from that run enters the ledger, and the
errors are mapped back to the holes whose generated code they land in:

```
warning: /tmp/demo/src/lib.rs:6: E0308: mismatched types: expected `i64`, found `&str`
re-asked for 1 answer(s) across 1 extra round(s) after the compile gate rejected them
```

`fill` re-asks for exactly those holes, quoting rustc at them, and re-gates.
Errors that land outside every generated region are the crate's own — they are
reported and not retried, since no answer can fix them.

Nothing is written until the gate accepts. That ordering matters most for
`--in-place`, where a write *is* the user's source: writing first and undoing on
rejection would leave a window in which the file on disk holds code that does not
compile. Because the write happens last, a rejected run leaves the source exactly
as it found it, and there is nothing to roll back.

`--no-verify` skips the gate. Because the ledger's one guarantee is that every
entry is code the gate accepted, a run with `--no-verify` **records nothing**.

### `build`

`fill` proves the generated code *type-checks*. `build` goes further: it compiles
the generated tree for real, so you can run it, test it, or hand its binary to
someone.

```
$ cargo hole build --path .
assembled .cargo-hole/build at /tmp/demo/.cargo-hole/build (4 file(s) copied, 0 reused, 2 artifacts overlaid)

building .cargo-hole/build with cargo
   Compiling demo v0.1.0 (/tmp/demo/.cargo-hole/build)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.82s
```

The reason this is a copy rather than a `cd .cargo-hole && cargo build` is that
**`.cargo-hole/` is not a crate.** It holds one generated `.rs` per source file
that *had a hole*, and nothing else — no `Cargo.toml`, and no file for a module
that had no holes. Building inside it cannot work.

So `build` mirrors the crate into `<root>/.cargo-hole/build/`, lays the artifacts
over the copy, and runs cargo there. The user's source is never opened for
writing, so there is no patch-and-restore window: a build can run for minutes,
be interrupted, and leave a binary behind, and none of that can touch the real
crate. That is why `build` does not reuse the gate's patch/restore machinery —
for a check that lasts seconds it is a fair trade, and for a build it is not.

Files whose contents already match are not rewritten, which keeps their mtimes
and therefore keeps cargo's incremental cache alive. A second `build` is
effectively free:

```
assembled .cargo-hole/build at /tmp/demo/.cargo-hole/build (0 file(s) copied, 4 reused, 2 artifacts overlaid)

building .cargo-hole/build with cargo
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.00s
```

#### The overlay is a symlink

Artifacts are not copied into the build tree — they are **linked** to
`.cargo-hole/patch/`. That matters because rustc reports errors against paths
inside the tree it compiled:

```
error[E0599]: no method named `parse` found for type `Value`
 --> /tmp/demo/.cargo-hole/build/src/util.rs:8:9
```

With a copy, that path names a duplicate. A user opens exactly the file rustc
told them about, fixes it, rebuilds — and the next `sync` silently overwrites the
edit with the patch tree's version. The fix vanishes, and nothing explains why.

With a link, both names are the same file. Editing the reported path edits the
generated code, and cargo's mtime check sees it, so the fix survives the rebuild.
`build` says when this applies:

```
note: 2 artifact(s) are linked into .cargo-hole/patch, so edits to the paths rustc
reports are edits to the generated code
```

Where symlinks are unavailable — a filesystem without them, or Windows without
the privilege — `build` falls back to copying and works identically, except that
edits then belong in `.cargo-hole/patch/`. The fallback is silent, because
warning on every run about a property of the user's filesystem would be noise.

`--clean` remains safe: `remove_dir_all` deletes the links, never their targets,
so the generated code is untouched.

Anything after the command goes to cargo unchanged, so `cargo hole build
--release`, `--features foo` and `-p member` all work without `cargo hole`
knowing about them. `--dry-run` assembles the tree and prints what it *would*
run; `--clean` deletes the tree first; `--build-dir` moves it.

A build that fails exits with cargo's status, so wrapping this in a script gives
the same answer `cargo build` would.

One caveat worth knowing: an unelaborated hole is still `todo!()`, whose type is
`!`, so a crate full of open holes **compiles**. `build` says so explicitly
rather than reporting a clean success:

```
warning: 2 hole(s) are still unelaborated in the build tree; run `cargo hole fill` first
```

For the same reason, if `build` ever found generated code it could not read — an
older `.cargo-hole/src/` layout, say — it refuses rather than building the crate
as-is, which would compile perfectly and quietly run with every hole
unimplemented.

Add `.cargo-hole/build/` to your `.gitignore`: it is a full second copy of the
crate plus its own `target/`.

### `run`

`run` assembles the same tree and then runs it, like `cargo run`:

```
$ cargo hole run --path .
assembled .cargo-hole/build at /tmp/demo/.cargo-hole/build (...)

running .cargo-hole/build with cargo
      Running `.cargo-hole/build/target/debug/demo`
```

It is not `build && ./target/debug/demo`, for two reasons.

**The working directory.** `cargo run` gives the program the directory cargo was
invoked in. Running it *inside* the build tree would therefore hand the program
the tree instead of your crate, so a relative path it opens — `./data/input.txt`,
a config next to the manifest, anything resolved at runtime — would silently point
at a copy. The copy usually contains those files, which is what makes this quiet
rather than loud: it works until the program writes, or until the two diverge.

So `run` keeps the crate's directory as the working directory and points cargo at
the tree's manifest instead. Output still lands in the tree's `target/`, so your
real `target/` stays untouched, exactly as with `build`.

**Target selection.** `cargo run` knows which target `--bin`, `-p` or `--example`
selected and where the result landed. Locating the binary by hand would be a
second implementation of cargo's own rules, and one that quietly disagrees the
moment a profile or target layout changes.

The program's exit status reaches the caller unchanged, so a script sees what
`cargo run` would have told it:

```
$ cargo hole run --path . ; echo $?
3
```

`--` separates the two kinds of argument, matching `cargo run`:

```
cargo hole run --path . --release --bin app   # these go to cargo
cargo hole run --path . -- --nocapture        # these go to the program
```

`--help` after the separator belongs to the program, not to `cargo hole` — which
is what you want when the program has its own flags. The other spelling works too:
anything after an explicit `--` is re-emitted to cargo behind a `--` of its own,
since `cargo run alpha` would otherwise read `alpha` as a cargo argument and fail.

`--clean`, `--build-dir` and `--dry-run` behave as they do for `build`.

### The ledger

Everything `cargo hole` writes lives under one directory, and each thing in it
has one job:

```
.cargo-hole/
├── patch/          generated code, mirroring the source tree
│   └── src/lib.rs
├── ledger.jsonl    every answer the compile gate accepted
├── .cargo-hole.probe.lock
└── build/          the shadow tree (disposable; safe to delete)
```

`patch/` holds the product; `build/` is derived from it and can be deleted at any
time — the next `build` recreates it. Keeping them apart means no command has to
guess which one it is looking at.

A second `fill` of an unchanged crate costs nothing:

```
$ cargo hole fill --path .
0 filled, 2 cached, 0 not filled, 0 skipped (0 model call(s))
```

Each answer is recorded in `<root>/.cargo-hole/ledger.jsonl`, one JSON object
per line, keyed by a blake3 hash of the hole's *meaning* -- the spec text with
whitespace collapsed, the enclosing signature, the `impl`/`trait` context and
the syntactic position. Line numbers are deliberately not part of the key, so
inserting a line above a hole, reordering functions, or running `cargo fmt` all
keep their answers; changing the spec or the signature does not.

The file is append-only: an update is a new line, so there is no
read-modify-write window in which two runs can lose each other's entries, and a
crash can only truncate the tail. The newest line for an id wins. Entries
written by a different schema version are ignored rather than misread --
regenerating one answer costs a model call, whereas misreading one could splice
the wrong code into a file.

Two properties are worth knowing:

- **Nothing is recorded until the generated tree has compiled.** Answers are
  held in memory, the whole tree is put through the compile gate
  (`src/verifier.rs`), and only then are the artifacts written and the accepted
  answers recorded. So an interrupted run never leaves behind a ledger entry for
  code that was never written, and *nothing* in the ledger is code that was never
  compiled. A run that fails the gate records nothing, which is why the next run
  has to generate those answers again -- see `--no-verify` below.
- **The gate is per tree, not per hole.** One hole's answer can depend on what
  another hole was filled with, so "this hole compiled" is not a property a
  single hole can have. One `cargo check` covers the whole generated tree, and a
  failure means no answer from that run is recorded.
- **A rejection names the answers at fault.** Errors are mapped back to the hole
  whose generated code they land in, so `fill` can re-ask for exactly those
  holes -- quoting rustc at them -- instead of discarding the whole run's work.
  Errors that land outside every generated region belong to the crate as it
  already was; they are reported rather than retried, because no answer can fix
  them.
- **A pin is never routed around.** A hole that has been pinned is regenerated
  (i.e. refused), never replayed from the ledger, even if an earlier run cached
  an answer for it before it was pinned. Otherwise the cache would quietly
  overwrite hand-written code.

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
environment layering, writing results in place or to `.cargo-hole/`, the ledger
that lets a repeated `fill` skip the model entirely, the batched type probe, the
compile gate that decides what may enter the ledger, `build`, which compiles the
generated tree in a shadow copy of the crate, and `run`, which runs it.

Not implemented yet, and therefore not relied upon by anything:

- **`cargo hole restore`.** The subcommand exists and panics with
  `not implemented` (exit 101). The probe lock, the `.cargo-hole.bak` files and
  `restore_leftovers` all exist and are tested, but nothing calls the function
  from the CLI.
- **`--at <file>:<line>`.** Accepted by the CLI and ignored; `--at` fills every
  hole anyway. (`--timeout` does now reach both the probe and the gate.)
- **Provider pluggability.** `docs/providers.md`, a `Provider` trait, a `script`
  provider and the `[model] provider/command/args` keys do not exist — the only
  agent is the `codex` CLI.
- **`Hole::hole_id`**, a blake3 hash over spec, signature, impl context,
  position and edition, is implemented but never called by anything.
- **`cargo hole type`**, a command that would report a hole's expected type
  without filling it. The probe can answer this; nothing exposes it yet.

## Development

```bash
cargo test            # 67 tests, all offline
cargo build
```

The suite is hermetic: the 67 tests live in `src/agent.rs` (10), `src/filler.rs`
(4), `src/storage.rs` (14), `src/storage/disk.rs` (11) and `src/render.rs` (28),
need no network, and there is no ignored/live-provider test group.

> Known flake: `temp_root()` builds a per-test directory out of the process id
> and a counter but never removes it, so a recycled pid can inherit a directory
> whose ledger already has lines and the `assert_eq!(lines.len(), 2)` checks
> fail. `rm -rf /tmp/cargo-hole-*` clears it. This predates the ledger work and
> is untouched by it.

### Layout

| module | responsibility |
|---|---|
| `hole.rs` | `syn`-based extraction of holes, spec parsing, pin markers, positions |
| `cmdline.rs` | the `clap` CLI: `list`, `fill`, `restore` |
| `filler.rs` | prompt assembly, reply extraction, splicing into the file |
| `storage.rs` | the store, the ledger entry format, and `Hole::hole_key` contracts |
| `storage/disk.rs` | the on-disk backend: atomic artifacts and an append-only ledger |
| `render.rs` | `list` output: the plain and pretty layouts, colour, wrapping, width |
| `agent.rs` | config file and `CARGO_HOLE_*` layering, agent selection |
| `agent/codex.rs` | driving `codex exec --json`, parsing its JSONL events |
| `util.rs` | file walk, byte-level scanner (delimiters, comments, strings), line/column |
| `prober.rs` | the type probe: patch a hole with `()`, read the `E0308`, restore |
| `verifier.rs` | the compile gate: check the whole generated tree, blame the answers at fault |
| `shadow.rs` | the shadow tree behind `build`/`run`: mirror the crate, link the artifacts over it, run cargo there |
| `main.rs`, `lib.rs` | thin entry point and module list |
