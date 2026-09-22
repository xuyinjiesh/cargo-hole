use std::{fs::canonicalize, path::PathBuf, unimplemented};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::render::{ColorMode, Env, RenderOptions, render_list};
use crate::{agent::Agent, filler::Filler, hole::Hole, storage::Storage, util::display_rel_path};
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
        println!(
            "no holes found in {}",
            display_rel_path(&root, &root),
        );
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

    let (mut filled, mut rejected, skipped) = (0usize, 0usize, 0usize);
    let mut cached = 0usize;
    let mut model_calls = 0u32;
    // Answers generated this run, held back until every file has been written.
    //
    // The ledger is a cache of answers that work, so an entry must not appear
    // until the generated tree it belongs to is on disk: a run that died
    // half-way would otherwise leave behind code nothing has ever compiled, and
    // a later run would replay it as though something had. Collecting the
    // answers here keeps that order -- artifacts first, ledger second -- and
    // leaves a single place to put the compile gate once `verifier.rs` exists.
    let mut pending: Vec<(String, String)> = Vec::new();

    for (file, mut file_holes) in tree_holes {
        let mut src = std::fs::read_to_string(&file)
            .with_context(|| format!("cannot read {}", file.display()))?;
        // Backwards, so earlier edits do not shift later byte offsets.
        file_holes.sort_by_key(|h| std::cmp::Reverse(h.byte_start));

        for hole in &file_holes {
            let key = hole.hash_key();

            // Pinned and unresolvable holes never consult the ledger. A pin
            // says the hand-written body is the authority, so replaying a
            // recorded answer -- which was generated for this key on some
            // earlier run, before anyone pinned it -- would silently route
            // around the pin. An unresolvable hole must not be patched at all,
            // which is what the field exists to stop. Both fall through to the
            // filler, which refuses them and reports why.
            if !hole.pinned && hole.unresolvable.is_none() {
                if let Some(code) = storage.get(&key) {
                    src.replace_range(hole.byte_start..hole.byte_end, code);
                    cached += 1;
                    continue;
                }
            }

            match filler.fill_hole_with(hole, Some(&expectations)) {
                Ok((code, calls)) => {
                    src.replace_range(hole.byte_start..hole.byte_end, code.as_str());
                    model_calls += calls;
                    filled += 1;
                    pending.push((key, code));
                }
                Err(e) => {
                    // One unfillable hole must not abandon the rest of the
                    // file: report it and keep going.
                    eprintln!("warning: skipping {}: {e:#}", hole.location());
                    rejected += 1;
                }
            }
        }
        
        if args.in_place {
            std::fs::write(&file, src)
                .with_context(|| format!("cannot write {}", file.display()))?;
        } else {
            // Through the store, so the artifact lands inside `.cargo-hole/`
            // and lands atomically -- a torn artifact would be read back by a
            // later run as though it were real output.
            storage.write_artifact(&file, &src)?;
        }
    }

    // Every artifact is on disk, so the answers are worth recording.
    for (key, code) in &pending {
        storage.put(key, code)?;
    }

    println!(
        "\n{filled} filled, {cached} cached, {rejected} not filled, {skipped} skipped \
         ({model_calls} model call(s))"
    );

    Ok(())
}
