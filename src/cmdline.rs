use std::{
    fs::canonicalize,
    path::{Path, PathBuf},
    unimplemented,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::render::{ColorMode, Env, RenderOptions, render_list};
use crate::verifier::{Patch, Verdict, Verifier};
use crate::{
    agent::Agent,
    filler::{Expectations, Filler},
    hole::Hole,
    prober::ProberOptions,
    storage::Storage,
    util::display_rel_path,
};
#[derive(Parser, Debug)]
#[command(
    name = "cargo-hole",
    bin_name = "cargo hole",
    version,
    about = "Find and fill `todo!(\"spec: ...\")` holes, using rustc as the type-query API"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List every hole in the crate, with its expected type and status.
    List(ListArgs),
    /// Implement holes using a configured model, verifying each with rustc.
    Fill(FillArgs),
    /// Restore any file left patched by an interrupted probe.
    Restore(RestoreArgs),
}

#[derive(Parser, Debug)]
struct ListArgs {
    /// Path to the crate root (defaults to the current directory).
    #[arg(long, default_value = ".")]
    path: PathBuf,
    /// Only report holes whose path contains this string.
    #[arg(long)]
    file: Option<String>,
    /// Exit non-zero if any `spec:` hole is still unelaborated.
    #[arg(long)]
    fail_on_unelaborated: bool,
    /// Maximum characters of each spec to display in the default layout.
    #[arg(long, default_value_t = 60)]
    width: usize,
    /// Group holes by file and show each one as a block, with the whole spec
    /// wrapped rather than truncated. Meant for reading.
    #[arg(long)]
    pretty: bool,
    /// When to colour the output.
    #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
    color: ColorMode,
}

#[derive(Parser, Debug)]
struct FillArgs {
    /// Path to the crate root (defaults to the current directory).
    #[arg(long, default_value = ".")]
    path: PathBuf,
    /// Only fill holes whose path contains this string.
    #[arg(long)]
    file: Option<String>,
    /// Fill only the hole at this `<file>:<line>`.
    #[arg(long)]
    at: Option<String>,
    /// Seconds to allow each `cargo check` and model call.
    #[arg(long, default_value_t = 300)]
    timeout: u64,

    /// Agent CLI to drive (`codex`), overriding the config file and
    /// `CARGO_HOLE_CLI`.
    #[arg(long)]
    cli: Option<String>,
    /// Model name, overriding the config file and `CARGO_HOLE_MODEL`.
    #[arg(long)]
    model: Option<String>,
    /// Base URL for the `openai` provider, overriding the config file and
    /// `CARGO_HOLE_BASE_URL`.
    #[arg(long)]
    base_url: Option<String>,
    /// API key for the `openai` provider, overriding the config file and
    /// `CARGO_HOLE_API_KEY`.
    #[arg(long)]
    api_key: Option<String>,

    /// Write the filled code back over the original files. Without it, results
    /// go to `<root>/.cargo-hole/` instead.
    #[arg(long)]
    in_place: bool,
    /// Skip the compile gate. Faster, but nothing is verified.
    #[arg(long)]
    no_verify: bool,
    /// Skip the type probe, so the model gets no expected type.
    #[arg(long)]
    no_probe: bool,
}

#[derive(Parser, Debug)]
struct RestoreArgs {
    /// Path to the crate root (defaults to the current directory).
    #[arg(long, default_value = ".")]
    path: PathBuf,
}

pub fn cargo_subcommand_argv<I>(args: I) -> Vec<std::ffi::OsString>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    let mut args: Vec<std::ffi::OsString> = args.into_iter().collect();
    if args.len() > 1 && args[1] == std::ffi::OsStr::new("hole") {
        args.remove(1);
    }
    args
}

pub fn cargo_hole_cli_main() -> Result<()> {
    let cli = Cli::parse_from(cargo_subcommand_argv(std::env::args_os()));
    match cli.command {
        Command::List(args) => list(args),
        Command::Fill(args) => fill(args),
        Command::Restore(_args) => unimplemented!(),
    }
}

/// Prober/verifier options for one run.
///
/// Built here rather than inside either of them so that `fill`'s `--timeout`
/// governs the probing check, the gate check, and the re-checks after a
/// rejection alike. Environment overrides still apply.
fn prober_options(timeout_secs: u64) -> ProberOptions {
    ProberOptions {
        timeout: std::time::Duration::from_secs(timeout_secs),
        ..ProberOptions::from_env()
    }
}

/// One file's work: the path, the source *as it was before anything was
/// written*, and each hole paired with the answer that went into it.
///
/// `None` as an answer means the hole was left unelaborated -- pinned,
/// unresolvable, or unfillable. It still compiles as `todo!()`, so it needs no
/// answer, but the gate has to see it in place.
///
/// Carrying the original text is what lets a retry rebuild the file without
/// reading it back. In `--in-place` mode the file on disk may already hold an
/// answer, and re-reading it would splice a second answer over the first.
#[derive(Debug, Clone)]
struct FileWork {
    file: PathBuf,
    original: String,
    answers: Vec<(Hole, Option<String>)>,
}

impl FileWork {
    /// The patch for the current answers, rebuilt from the untouched original.
    fn patch(&self) -> Result<Patch> {
        build_patch(&self.file, &self.original, &self.answers)
    }

    /// The text that should end up on disk, if the gate accepts it.
    fn checked(&self) -> Result<String> {
        Ok(self.patch()?.checked)
    }
}

/// How many times to re-ask after the gate rejects a tree.
///
/// Bounded so a hole the model simply cannot get right terminates rather than
/// burning calls forever. Each round re-runs the gate once, so the cost of
/// raising this is a `cargo check` per round.
const MAX_GATE_ROUNDS: usize = 2;

/// Assemble the checked text for one file, and remember where each answer
/// landed.
///
/// The text and the regions must come out of the same splice -- an answer of a
/// different length than the `todo!(...)` it replaced moves every hole after it,
/// so regions computed separately would blame the wrong hole. [`Patch::splice`]
/// is what enforces that.
fn build_patch(file: &Path, src: &str, answers: &[(Hole, Option<String>)]) -> Result<Patch> {
    let pairs: Vec<(&Hole, Option<&str>)> = answers
        .iter()
        .map(|(h, code)| (h, code.as_deref()))
        .collect();
    Patch::splice(file, src, &pairs)
        .map_err(|e| anyhow::anyhow!("cannot assemble {}: {e}", file.display()))
}

/// After the gate rejects a tree, drop the answers it blamed and ask again for
/// just those, quoting rustc at them.
///
/// Nothing is written to disk here. The patches are rebuilt from each file's
/// *original* text, which is what makes a retry safe in `--in-place` mode: the
/// file on disk still holds the holes as the user wrote them, because the write
/// happens only once the gate has accepted.
///
/// Returns the number of answers re-asked, the number of extra rounds, and the
/// verdict the last gate run reached.
#[allow(clippy::too_many_arguments)]
fn retry_rejected(
    verifier: &Verifier,
    filler: &Filler<'_>,
    expectations: &Expectations,
    work: &mut [FileWork],
    patches: &mut [Patch],
    verdict: Verdict,
    pending: &mut Vec<(String, String)>,
    model_calls: &mut u32,
) -> Result<(usize, usize, Verdict)> {
    let mut verdict = verdict;
    let mut retried = 0usize;
    let mut rounds = 0usize;

    loop {
        // Which holes are at fault this round, keyed by ledger key -- the same
        // key `pending` uses, so a rejected answer can be dropped from it
        // directly and identity is decided in one place.
        let mut blame: std::collections::HashMap<String, Vec<String>> = Default::default();
        for f in verdict.attributable() {
            let Some(hole) = &f.hole else { continue };
            blame
                .entry(hole.hash_key())
                .or_default()
                .push(f.message.clone());
        }
        if blame.is_empty() {
            // Nothing to re-ask: the remaining errors are the crate's own, and
            // no answer can fix them.
            break;
        }
        if rounds >= MAX_GATE_ROUNDS {
            eprintln!(
                "warning: giving up after {MAX_GATE_ROUNDS} round(s); {} answer(s) still do not \
                 compile",
                blame.len()
            );
            break;
        }
        rounds += 1;
        retried += blame.len();

        // A rejected answer must not be recorded, whatever else happens.
        pending.retain(|(key, _)| !blame.contains_key(key));

        for item in work.iter_mut() {
            for (hole, code) in item.answers.iter_mut() {
                let Some(errors) = blame.get(&hole.hash_key()) else {
                    continue;
                };
                match filler.refill_hole(hole, Some(expectations), errors) {
                    Ok((new_code, calls)) => {
                        *model_calls += calls;
                        pending.push((hole.hash_key(), new_code.clone()));
                        *code = Some(new_code);
                    }
                    Err(e) => {
                        eprintln!("warning: still cannot fill {}: {e:#}", hole.location());
                        // Left unelaborated, which compiles, so one hole the
                        // model cannot do does not sink the whole tree.
                        *code = None;
                    }
                }
            }

            // Rebuild this file's patch from its original text. Still nothing
            // written.
            let patch = item.patch()?;
            if let Some(slot) = patches.iter_mut().find(|p| p.file == item.file) {
                *slot = patch;
            }
        }

        verdict = verifier.verify(patches);
        if verdict.is_accepted() {
            break;
        }
    }

    Ok((retried, rounds, verdict))
}

fn list(args: ListArgs) -> Result<()> {
    let root = canonicalize(&args.path)
        .with_context(|| format!("fail to canonicalize {}", args.path.display()))?;

    let holes = Hole::list_holes(&root)?
        .into_iter()
        .filter(|hole| {
            if let Some(specified_file) = &args.file {
                if let Some(hole_file) = hole.file.file_name() {
                    if &hole_file.to_string_lossy() == specified_file {
                        return true;
                    }
                }
                return false;
            }
            return true;
        })
        .collect::<Vec<_>>();

    // Rendering is a pure function of the holes and the environment, so `list`
    // only has to resolve the options and print. Pinned and unresolvable are
    // counted as independent sets: a hole can be both, and subtracting them
    // from the total separately would underflow.
    let opts = RenderOptions::resolve(args.pretty, args.color, &Env::detect(), args.width);
    print!("{}", render_list(&holes, &root, &opts));

    if args.fail_on_unelaborated {
        anyhow::bail!(
            "{} unelaborated hole(s) present and --fail-on-unelaborated was given",
            holes.len()
        );
    }
    Ok(())
}

fn fill(args: FillArgs) -> Result<()> {
    let root = canonicalize(&args.path)
        .with_context(|| format!("fail to canonicalize {}", args.path.display()))?;
    let mut storage = Storage::open(&root)?;

    let holes = Hole::list_holes(&root)?
        .into_iter()
        .filter(|hole| {
            if let Some(specified_file) = &args.file {
                if let Some(hole_file) = hole.file.file_name() {
                    if &hole_file.to_string_lossy() == specified_file {
                        return true;
                    }
                }
                return false;
            }
            return true;
        })
        .collect::<Vec<_>>();

    if holes.is_empty() {
        println!("no holes found in {}", display_rel_path(&root, &root),);
        return Ok(());
    }

    let agent = Agent::from_args(
        args.cli.as_deref(),
        &root,
        args.model.as_deref(),
        args.base_url.as_deref(),
        args.api_key.as_deref(),
    )?;
    println!(
        "filling {} hole(s) in {} using {}",
        holes.len(),
        display_rel_path(&root, &root),
        agent.label(),
    );
    let filler = if args.no_probe {
        Filler::without_probe(root.clone(), &agent)
    } else {
        Filler::new(root.clone(), &agent)
    };
    // The gate and the prober share one options value, so the timeout, offline
    // mode and target directory are the same for both. `--timeout` reaches it
    // here -- it used to be parsed and dropped on the floor.
    let verifier = Verifier::new(root.clone(), prober_options(args.timeout));

    // Probe everything up front, so the whole run costs one `cargo check` per
    // file rather than one per hole. This has to happen before any file is
    // written, since probing patches and restores each file in place.
    //
    // Only holes that will actually be filled are probed: a pinned hole is never
    // regenerated, and an unresolvable one is never patched, so neither needs a
    // type. A single hole is left to `fill_hole`, which probes it just as well
    // without paying for the batch machinery.
    let needs_probe: Vec<Hole> = holes
        .iter()
        .filter(|h| !h.pinned && h.unresolvable.is_none())
        .cloned()
        .collect();
    let expectations = if args.no_probe || needs_probe.len() < 2 {
        Default::default()
    } else {
        // The file count is the useful number: it is how many `cargo check` runs
        // this is about to cost.
        let files: std::collections::BTreeSet<&PathBuf> =
            needs_probe.iter().map(|h| &h.file).collect();
        println!(
            "probing {} hole(s) in {} file(s) for expected types",
            needs_probe.len(),
            files.len()
        );
        filler.probe_all(&needs_probe)
    };

    // Group by file so each file is read and written once.
    let mut tree_holes: std::collections::BTreeMap<PathBuf, Vec<Hole>> = Default::default();
    for hole in holes {
        tree_holes.entry(hole.file.clone()).or_default().push(hole);
    }

    let (mut filled, mut rejected, mut skipped) = (0usize, 0usize, 0usize);
    let mut cached = 0usize;
    let mut model_calls = 0u32;
    // Answers generated this run, held back until the compile gate accepts the
    // tree they belong to.
    //
    // The ledger is a cache of answers that *work*, so an entry must not appear
    // until the generated tree has compiled. A run that recorded an unverified
    // answer would cache it permanently: every later run would replay it as
    // though something had checked it.
    let mut pending: Vec<(String, String)> = Vec::new();
    // One patch per file, ready for the gate. Holds the whole generated text,
    // not just the answers, because the gate has to check every answer in the
    // tree -- replayed and generated alike, since they can depend on each other.
    let mut patches: Vec<Patch> = Vec::new();
    // Where each answer sits, so a rejected one can be re-asked. Kept beside the
    // patch list rather than inside it because it is `fill`'s business, not the
    // gate's.
    let mut work: Vec<FileWork> = Vec::new();

    for (file, mut file_holes) in tree_holes {
        let src = std::fs::read_to_string(&file)
            .with_context(|| format!("cannot read {}", file.display()))?;
        // Ascending, which is the order `Patch::splice` wants: it does the
        // splicing in one pass and returns each answer's position, so the old
        // trick of replacing backwards is no longer needed -- and no longer
        // correct, because the regions have to come out of the same splice.
        file_holes.sort_by_key(|h| h.byte_start);

        let mut answers: Vec<(Hole, Option<String>)> = Vec::with_capacity(file_holes.len());

        for hole in &file_holes {
            let key = hole.hash_key();

            // Pinned and unresolvable holes never consult the ledger. A pin
            // says the hand-written body is the authority, so replaying a
            // recorded answer -- which was generated for this key on some
            // earlier run, before anyone pinned it -- would silently route
            // around the pin. An unresolvable hole must not be patched at all,
            // which is what the field exists to stop. Both stay unelaborated,
            // and are reported below.
            if hole.pinned || hole.unresolvable.is_some() {
                answers.push((hole.clone(), None));
                skipped += 1;
                continue;
            }

            if let Some(code) = storage.get(&key) {
                cached += 1;
                answers.push((hole.clone(), Some(code.to_string())));
                continue;
            }

            match filler.fill_hole_with(hole, Some(&expectations)) {
                Ok((code, calls)) => {
                    model_calls += calls;
                    filled += 1;
                    pending.push((key, code.clone()));
                    answers.push((hole.clone(), Some(code)));
                }
                Err(e) => {
                    // One unfillable hole must not abandon the rest of the
                    // file: report it, leave it as `todo!()`, and keep going.
                    // Left unelaborated it still compiles, so it does not fail
                    // the gate for the holes that did get an answer.
                    eprintln!("warning: skipping {}: {e:#}", hole.location());
                    rejected += 1;
                    answers.push((hole.clone(), None));
                }
            }
        }

        let item = FileWork {
            file,
            original: src,
            answers,
        };
        patches.push(item.patch()?);
        work.push(item);
    }

    // The gate, then the writes. Nothing is written until the generated tree has
    // compiled, because in `--in-place` mode a write *is* the user's source: a
    // rejected answer left behind would be worse than not filling the hole at
    // all. Keeping the write last also means a rejection leaves no trace to undo.
    let mut retried = 0usize;
    let mut rounds = 0usize;
    let mut verdict = if args.no_verify {
        // Reported rather than silent: without the gate nothing here was
        // compiled, so nothing may be cached either.
        eprintln!(
            "warning: --no-verify skips the compile gate, so no answers were recorded; \
             the next run will have to generate them again"
        );
        pending.clear();
        Verdict::Accepted
    } else {
        verifier.verify(&patches)
    };

    if !args.no_verify && verdict.is_rejected() {
        // Errors the gate could not pin on a generated answer belong to the crate
        // as it already was. Asking the model again would never fix them.
        for f in verdict.pre_existing() {
            eprintln!(
                "warning: {} is not a generated answer, so it was already broken; fix it and \
                 re-run",
                f.label()
            );
        }
        let outcome = retry_rejected(
            &verifier,
            &filler,
            &expectations,
            &mut work,
            &mut patches,
            verdict,
            &mut pending,
            &mut model_calls,
        )?;
        (retried, rounds, verdict) = outcome;
    }

    let accepted = verdict.is_accepted();

    if accepted {
        // On disk last, now that the tree is known good.
        for item in &work {
            let checked = item.checked()?;
            if args.in_place {
                std::fs::write(&item.file, &checked)
                    .with_context(|| format!("cannot write {}", item.file.display()))?;
            } else {
                // Through the store, so the artifact lands inside
                // `.cargo-hole/` and lands atomically -- a torn artifact would
                // be read back by a later run as though it were real output.
                storage.write_artifact(&item.file, &checked)?;
            }
        }
        // Artifacts are on disk, so the answers are worth recording.
        for (key, code) in &pending {
            storage.put(key, code)?;
        }
    } else {
        // Nothing was written and nothing may be recorded. Say which answers
        // were the problem, so the run is actionable rather than just failed.
        pending.clear();
        for f in verdict.attributable() {
            eprintln!("error: {} did not compile and was not written", f.label());
        }
        if verdict.attributable().is_empty() {
            eprintln!(
                "error: the crate did not compile, and not because of a generated answer; \
                 nothing was written"
            );
        }
    }

    println!(
        "\n{filled} filled, {cached} cached, {rejected} not filled, {skipped} skipped \
         ({model_calls} model call(s))"
    );
    if retried > 0 {
        println!(
            "re-asked for {retried} answer(s) across {rounds} extra round(s) after the compile \
             gate rejected them"
        );
    }
    if !pending.is_empty() {
        println!("{} answer(s) recorded in the ledger", pending.len());
    }

    Ok(())
}
