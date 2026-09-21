use std::{fs::canonicalize, path::PathBuf, unimplemented};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::{agent::Agent, filler::Filler, hole::Hole, util::display_rel_path};
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
    /// Maximum characters of each spec to display.
    #[arg(long, default_value_t = 60)]
    width: usize,
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

    if holes.is_empty() {
        println!("no holes found in {}", display_rel_path(&root, &root));
        return Ok(());
    }

    let pinned = holes.iter().filter(|h| h.pinned).count();
    let unresolvable = holes.iter().filter(|h| h.unresolvable.is_some()).count();

    println!(
        "{} hole(s) in {}", 
        holes.len(), root.display(),
    );
    println!();

    for (i, hole) in holes.iter().enumerate() {
        let status = if hole.unresolvable.is_some() {
            "UNRESOLVABLE"
        } else if hole.pinned {
            "PINNED"
        } else {
            "open"
        };
        println!(
            "[{:>2}] {}:{}  {}  ({}, {})",
            i + 1,
            display_rel_path(&root, &hole.file),
            hole.line,
            status,
            hole.macro_name,
            hole.position.as_str(),
        );
        println!("     fn:   {}", hole.fn_sig);
        println!("     spec: {}", hole.spec_summary(args.width));
        if let Some(reason) = &hole.unresolvable {
            println!("     why:  {reason}");
        }
        println!();
    }

    println!(
        "summary: {} open, {} pinned, {} unresolvable",
        holes.len() - pinned - unresolvable,
        pinned,
        unresolvable
    );
    if pinned > 0 {
        println!(
            "note: {pinned} pinned hole(s) are hand-written and will never be regenerated; \
             they are tracked technical debt."
        );
    }

    if args.fail_on_unelaborated && !holes.is_empty() {
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
    let filler = Filler::new(root.clone(), &agent);

    // Group by file so each file is read and written once.
    let mut tree_holes: std::collections::BTreeMap<PathBuf, Vec<Hole>> = Default::default();
    for hole in holes {
        tree_holes.entry(hole.file.clone()).or_default().push(hole);
    }

    let (mut filled, mut rejected, skipped) = (0usize, 0usize, 0usize);
    let mut model_calls = 0u32;

    for (file, mut file_holes) in tree_holes {
        let mut src = std::fs::read_to_string(&file)
            .with_context(|| format!("cannot read {}", file.display()))?;
        // Backwards, so earlier edits do not shift later byte offsets.
        file_holes.sort_by_key(|h| std::cmp::Reverse(h.byte_start));

        for hole in &file_holes {
            match filler.fill_holes(hole, &src) {
                Ok((patched, calls)) => {
                    src = patched;
                    model_calls += calls;
                    filled += 1;
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
            std::fs::write(file, src)?;
        } else {
            let rel = file.strip_prefix(&root)?;
            let file = root.join(".cargo-hole").join(rel);
            if let Some(dir) = file.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("cannot create {}", dir.display()))?;
            }
            std::fs::write(file, src)?;
        }
    }

    println!(
        "\n{filled} filled, {rejected} not filled, {skipped} skipped ({model_calls} model call(s))"
    );

    Ok(())
}
