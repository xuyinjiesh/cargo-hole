//! Turning a list of holes into text for a terminal.
//!
//! Rendering is a pure function of the holes, the root and the options, so the
//! layout can be tested by asserting on a string instead of capturing stdout.
//! [`render_list`] is the only entry point the CLI uses.
//!
//! Two layouts share one model of a hole:
//!
//! - **plain**, the default, is the line-oriented format scripts already parse.
//!   Its text is deliberately unchanged; only colour is added, and only when a
//!   human is watching.
//! - **pretty** (`--pretty`) groups holes by file and gives each one a small
//!   block, which is what you want when reading rather than grepping.
//!
//! Colour is never load-bearing. Every distinction it draws is also spelled out
//! in the text, so `--color never`, a pipe and a dumb terminal all lose nothing
//! but the emphasis.

use std::fmt::Write as _;
use std::io::IsTerminal;
use std::path::Path;

use crate::hole::Hole;
use crate::util::display_rel_path;

/// Wrap width assumed when the terminal does not say how wide it is.
pub const DEFAULT_WIDTH: usize = 100;
/// Below this, columns are too cramped for the block layout to read as blocks.
pub const MIN_WIDTH: usize = 40;
/// Above this, prose becomes hard to track back to the next line.
pub const MAX_WIDTH: usize = 200;

/// When to use ANSI colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorMode {
    /// Colour a terminal, stay plain when piped. The default.
    Auto,
    /// Always colour, for CI that captures output but shows it to a human.
    Always,
    /// Never colour.
    Never,
}

/// The few environment facts that affect rendering.
///
/// Gathered into a value rather than read at each use so that the decisions
/// built on it -- colour on or off, how wide to wrap -- are pure and testable.
/// Reading the process environment inside a test would also need `set_var`,
/// which is `unsafe` in edition 2024 and cannot be done safely while other
/// threads run.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// Whether stdout is a terminal.
    pub is_tty: bool,
    /// `NO_COLOR`, if set to a non-empty value. The de-facto opt-out.
    pub no_color: Option<String>,
    /// `TERM`, to catch `dumb`.
    pub term: Option<String>,
    /// `COLUMNS`, as a hint at the terminal's width.
    pub columns: Option<String>,
}

impl Env {
    /// Read the real environment.
    pub fn detect() -> Env {
        fn var(name: &str) -> Option<String> {
            std::env::var(name).ok().filter(|v| !v.is_empty())
        }
        Env {
            is_tty: std::io::stdout().is_terminal(),
            no_color: var("NO_COLOR"),
            term: var("TERM"),
            columns: var("COLUMNS"),
        }
    }
}

/// Whether `mode` calls for colour in `env`.
///
/// `Always` really means always: an explicit flag is a stronger signal than any
/// environment guess, so `NO_COLOR` does not veto `--color always`. It does veto
/// `Auto`, because a program that has not been asked for colour should respect
/// the request not to have it.
pub fn color_enabled(mode: ColorMode, env: &Env) -> bool {
    match mode {
        ColorMode::Always => true,
        ColorMode::Never => false,
        ColorMode::Auto => {
            env.is_tty
                && env.no_color.is_none()
                // `dumb` terminals do not interpret escapes, and would show them
                // as literal `[1m` noise.
                && env.term.as_deref() != Some("dumb")
        }
    }
}

/// How wide to wrap, from `COLUMNS` when it is usable.
///
/// Clamped rather than trusted: a stale or nonsensical `COLUMNS` should not
/// produce one-word-per-line output or an unreadably long line.
pub fn render_width(env: &Env) -> usize {
    env.columns
        .as_deref()
        .and_then(|c| c.trim().parse::<usize>().ok())
        .filter(|w| *w > 0)
        .unwrap_or(DEFAULT_WIDTH)
        .clamp(MIN_WIDTH, MAX_WIDTH)
}

/// How to render, with the environment already resolved.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// Use the block layout instead of the line-oriented one.
    pub pretty: bool,
    /// Emit ANSI escapes.
    pub color: bool,
    /// Wrap width for the block layout.
    pub wrap_width: usize,
    /// Characters of spec text to show in the line-oriented layout, which
    /// truncates rather than wraps.
    pub spec_width: usize,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions {
            pretty: false,
            color: false,
            wrap_width: DEFAULT_WIDTH,
            spec_width: 60,
        }
    }
}

impl RenderOptions {
    /// Resolve `--pretty` and `--color` against `env`.
    ///
    /// `spec_width` is passed through from `--width`, which the line-oriented
    /// layout has always used to bound a spec's one-line summary.
    pub fn resolve(pretty: bool, mode: ColorMode, env: &Env, spec_width: usize) -> RenderOptions {
        RenderOptions {
            pretty,
            color: color_enabled(mode, env),
            wrap_width: render_width(env),
            spec_width,
        }
    }
}

/// What a hole's status is, as a reader should understand it.
///
/// Pinned and unresolvable are tracked separately because they are set
/// independently: a pinned hole whose byte range could not be resolved is both
/// facts at once, and collapsing them into one status would hide whichever
/// check ran second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    /// Hand-written, never regenerated.
    pub pinned: bool,
    /// Found, but its byte range could not be trusted.
    pub unresolvable: bool,
}

impl Status {
    /// The status of `hole`.
    pub fn of(hole: &Hole) -> Status {
        Status {
            pinned: hole.pinned,
            unresolvable: hole.unresolvable.is_some(),
        }
    }

    /// The word shown in the status column.
    ///
    /// Lower case in the pretty layout, where the column is read as a label
    /// rather than shouted.
    pub fn word(self) -> &'static str {
        match (self.pinned, self.unresolvable) {
            (false, false) => "open",
            (true, false) => "pinned",
            (false, true) => "unresolvable",
            (true, true) => "pinned+unresolved",
        }
    }

    /// The upper-case word used by the plain layout, which predates the
    /// distinction and keeps its own spelling.
    pub fn plain_word(self) -> &'static str {
        if self.unresolvable {
            "UNRESOLVABLE"
        } else if self.pinned {
            "PINNED"
        } else {
            "open"
        }
    }

    /// The colour this status deserves.
    ///
    /// Yellow for `open`: unfinished work, which is expected, not a fault. Cyan
    /// for pinned: a deliberate decision. Red for unresolvable: something the
    /// tool cannot act on, which is the only case that needs attention now.
    fn style(self) -> Style {
        match (self.pinned, self.unresolvable) {
            (false, false) => Style::Yellow,
            (true, false) => Style::Cyan,
            (false, true) | (true, true) => Style::Red,
        }
    }
}

/// The handful of ANSI styles this module uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Style {
    Bold,
    Dim,
    Yellow,
    Cyan,
    Red,
}

impl Style {
    fn code(self) -> &'static str {
        match self {
            Style::Bold => "\x1b[1m",
            Style::Dim => "\x1b[2m",
            Style::Yellow => "\x1b[33m",
            Style::Cyan => "\x1b[36m",
            Style::Red => "\x1b[31m",
        }
    }
}

const RESET: &str = "\x1b[0m";

/// Render `holes` for display.
///
/// The result ends with a newline and is meant to be printed as-is.
pub fn render_list(holes: &[Hole], root: &Path, opts: &RenderOptions) -> String {
    if holes.is_empty() {
        return format!("no holes found in {}\n", display_rel_path(root, root));
    }
    if opts.pretty {
        render_pretty(holes, root, opts)
    } else {
        render_plain(holes, root, opts)
    }
}

/// Paint `text` with `style` when colour is on.
fn paint(text: &str, style: Style, color: bool) -> String {
    if color {
        format!("{}{text}{RESET}", style.code())
    } else {
        text.to_string()
    }
}

/// The plain layout: one block of lines per hole, unchanged since before colour
/// was an option.
///
/// Its shape is load-bearing for anything that greps the output, so it is kept
/// exactly as it was. Colour is deliberately not applied here: the caller asks
/// for it only alongside `--pretty`, and this format exists for scripts.
fn render_plain(holes: &[Hole], root: &Path, opts: &RenderOptions) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{} hole(s) in {}", holes.len(), root.display());
    let _ = writeln!(out);

    for (i, hole) in holes.iter().enumerate() {
        let status = Status::of(hole);
        let _ = writeln!(
            out,
            "[{:>2}] {}:{}  {}  ({}, {})",
            i + 1,
            display_rel_path(root, &hole.file),
            hole.line,
            status.plain_word(),
            hole.macro_name,
            hole.position.as_str(),
        );
        let _ = writeln!(out, "     fn:   {}", hole.fn_sig);
        let _ = writeln!(out, "     spec: {}", hole.spec_summary(opts.spec_width));
        if let Some(reason) = &hole.unresolvable {
            let _ = writeln!(out, "     why:  {reason}");
        }
        let _ = writeln!(out);
    }

    write_summary(&mut out, holes);
    out
}

/// The block layout: holes grouped by file, one small block each.
fn render_pretty(holes: &[Hole], root: &Path, opts: &RenderOptions) -> String {
    let count = holes.len();
    let open = holes
        .iter()
        .filter(|h| {
            let s = Status::of(h);
            !s.pinned && !s.unresolvable
        })
        .count();
    let pinned = holes.iter().filter(|h| h.pinned).count();
    let unresolvable = holes.iter().filter(|h| h.unresolvable.is_some()).count();

    let mut out = String::new();

    // Header: the crate's name carries the eye, the full path is reference.
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());
    let name = paint(&name, Style::Bold, opts.color);

    // "hole(s)" reads as a stutter next to a number; agree with the count.
    let noun = if count == 1 { "hole" } else { "holes" };
    let mut tally = Vec::new();
    for (n, word, style) in [
        (open, "open", Style::Yellow),
        (pinned, "pinned", Style::Cyan),
        (unresolvable, "unresolvable", Style::Red),
    ] {
        if n > 0 {
            tally.push(paint(&format!("{n} {word}"), style, opts.color));
        }
    }
    let _ = writeln!(out, "{name} — {count} {noun}: {}", tally.join(" · "));
    let _ = writeln!(
        out,
        "{}",
        paint(&root.display().to_string(), Style::Dim, opts.color)
    );

    // Columns sized from the data, so a crate with no long status does not get
    // a gutter of empty space.
    let line_width = holes
        .iter()
        .map(|h| h.line.to_string().len())
        .max()
        .unwrap_or(1)
        .max(2);
    let label_width = holes
        .iter()
        .map(|h| Status::of(h).word().len())
        .max()
        .unwrap_or(4)
        .max("spec".len());

    // The gutter a field line starts at: two spaces, the line number, two
    // more, then the status column. Field text then sits a further
    // `label_width + 2` along. Deriving the wrap width from this same number,
    // rather than from a separately computed indent, is what keeps a wrapped
    // line inside the terminal.
    let gutter = 2 + line_width + 2 + label_width;
    let content_width = opts
        .wrap_width
        .saturating_sub(gutter + label_width + 2)
        .max(20);
    let pad = " ".repeat(gutter);

    // Grouped by file, preserving the order the holes arrived in (already
    // sorted by file then position).
    let mut current: Option<&Path> = None;
    for hole in holes {
        if current != Some(hole.file.as_path()) {
            if current.is_some() {
                let _ = writeln!(out);
            }
            current = Some(hole.file.as_path());
            let rel = display_rel_path(root, &hole.file);
            let _ = writeln!(out, "  {}", paint(&rel, Style::Bold, opts.color));
        }

        let status = Status::of(hole);
        let label = format!("{:width$}", status.word(), width = label_width);
        // The label is padded so the column lines up, but only when colour is
        // off: a trailing run of spaces is invisible in a terminal and shows up
        // in every diff and `cat -A` of the output, so it is worth not emitting.
        let status_text = paint(&label, status.style(), opts.color);
        let status_text = if opts.color {
            status_text
        } else {
            status_text.trim_end().to_string()
        };
        let _ = writeln!(out, "  {:>line_width$}  {status_text}", hole.line);

        // The signature, then whatever else is worth saying about the hole.
        // Emitted only when there is a signature to show: a hole in item
        // position may have none, and an empty line would look broken.
        // Labelled `sig` rather than `fn` because the text already starts with
        // the `fn` keyword: `fn  fn double(..)` reads as a stutter.
        if !hole.fn_sig.trim().is_empty() {
            write_field(&mut out, "sig", hole.fn_sig.trim(), &pad, content_width);
        }
        if let Some(ctx) = &hole.impl_ctx {
            write_field(&mut out, "ctx", ctx, &pad, content_width);
        }
        write_field(&mut out, "spec", hole.spec.trim(), &pad, content_width);
        if let Some(reason) = &hole.unresolvable {
            write_field(&mut out, "why", reason, &pad, content_width);
        }
        // Position and macro are only worth a line when they are surprising.
        // "expression position" is the norm; a hole hidden in a macro body is
        // the case that explains odd behaviour later.
        match hole.position {
            crate::hole::HolePosition::MacroBody => write_field(
                &mut out,
                "note",
                &format!(
                    "inside a `{}!` body, so rustc sees it as raw tokens",
                    hole.macro_name
                ),
                &pad,
                content_width,
            ),
            crate::hole::HolePosition::Unknown => write_field(
                &mut out,
                "note",
                "position could not be determined",
                &pad,
                content_width,
            ),
            _ => {}
        }
        let _ = writeln!(out);
    }

    // The header already carried the tally, so the only thing left to say is
    // the caveat the pinned count implies.
    if pinned > 0 {
        let _ = writeln!(
            out,
            "  {pinned} pinned hole(s) are hand-written and will never be \
             regenerated; they are tracked technical debt."
        );
    }
    out
}

/// Write one `label  text` field, wrapping `text` and indenting continuations.
fn write_field(out: &mut String, label: &str, text: &str, pad: &str, content_width: usize) {
    let lines = wrap(text, content_width);
    let mut lines = lines.into_iter();
    match lines.next() {
        None => {
            let _ = writeln!(out, "{pad}{label}");
        }
        Some(first) => {
            let _ = writeln!(out, "{pad}{label}  {first}");
        }
    }
    for rest in lines {
        let _ = writeln!(out, "{pad}{}  {rest}", " ".repeat(label.len()));
    }
}

/// The trailing summary in both layouts.
fn write_summary(out: &mut String, holes: &[Hole]) {
    let open = holes
        .iter()
        .filter(|h| !h.pinned && h.unresolvable.is_none())
        .count();
    let pinned = holes.iter().filter(|h| h.pinned).count();
    let unresolvable = holes.iter().filter(|h| h.unresolvable.is_some()).count();

    let _ = writeln!(
        out,
        "summary: {open} open, {pinned} pinned, {unresolvable} unresolvable"
    );
    if pinned > 0 {
        let _ = writeln!(
            out,
            "note: {pinned} pinned hole(s) are hand-written and will never be regenerated; \
             they are tracked technical debt."
        );
    }
}

/// How many terminal cells `c` occupies.
///
/// A small `wcwidth` rather than a dependency: the crate's only source of
/// Unicode is a spec someone typed, and the case that actually matters is a
/// CJK spec, whose characters are two cells wide. Counting every character as
/// one would wrap those lines short and, worse, mis-pad every aligned column
/// to their right.
///
/// Ambiguous-width characters (the box-drawing and geometric shapes an earlier
/// draft used for bullets) are treated as one cell, which is correct in a
/// Western terminal and wrong in a CJK one. They are not used here for exactly
/// that reason: the layout is plain ASCII, so it lines up everywhere.
fn char_width(c: char) -> usize {
    let cp = c as u32;
    // Combining marks and other zero-width formatting characters.
    if c.is_control()
        || (0x0300..=0x036F).contains(&cp)
        || (0x200B..=0x200F).contains(&cp)
        || (0xFE00..=0xFE0F).contains(&cp)
    {
        return 0;
    }
    // East Asian Wide and Fullwidth.
    if matches!(cp,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE10..=0xFE19
        | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x1F300..=0x1F64F
        | 0x1F900..=0x1F9FF
        | 0x20000..=0x3FFFD
    ) {
        return 2;
    }
    1
}

/// The width of `s` in terminal cells.
fn text_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Break `text` into lines of at most `width` cells.
///
/// Greedy on whitespace, so prose wraps at word boundaries and keeps its
/// indentation meaningful. A single word longer than the line -- a path, a URL,
/// or a CJK run with no spaces at all -- is broken at the last character that
/// fits, since the alternative is a line that overflows the terminal.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_width = 0usize;

    for word in text.split_whitespace() {
        let word_width = text_width(word);
        let sep = usize::from(!line.is_empty());
        if line_width + sep + word_width <= width {
            if sep == 1 {
                line.push(' ');
                line_width += 1;
            }
            line.push_str(word);
            line_width += word_width;
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
            line_width = 0;
        }
        if word_width <= width {
            line.push_str(word);
            line_width = word_width;
            continue;
        }
        // Too long to fit on a line of its own: hard-break it.
        let mut chunk = String::new();
        let mut chunk_width = 0usize;
        for c in word.chars() {
            let w = char_width(c);
            if chunk_width + w > width {
                lines.push(std::mem::take(&mut chunk));
                chunk_width = 0;
            }
            chunk.push(c);
            chunk_width += w;
        }
        if !chunk.is_empty() {
            line = chunk;
            line_width = chunk_width;
        }
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hole::HolePosition;
    use std::path::PathBuf;

    /// A hole with everything at a sensible default.
    fn hole(spec: &str) -> Hole {
        Hole {
            file: PathBuf::from("/tmp/crate/src/lib.rs"),
            byte_start: 0,
            byte_end: 20,
            spec: spec.to_string(),
            fn_sig: "pub fn double(n: i64) -> i64".to_string(),
            fn_name: "double".to_string(),
            impl_ctx: None,
            line: 12,
            column: 5,
            macro_name: "todo".to_string(),
            position: HolePosition::Expression,
            pinned: false,
            unresolvable: None,
        }
    }

    fn opts(pretty: bool) -> RenderOptions {
        RenderOptions {
            pretty,
            color: false,
            wrap_width: 100,
            spec_width: 60,
        }
    }

    fn render(holes: &[Hole], pretty: bool) -> String {
        render_list(holes, Path::new("/tmp/crate"), &opts(pretty))
    }

    #[test]
    fn no_holes_is_reported_in_the_same_words_in_both_layouts() {
        let expected = "no holes found in .\n";
        assert_eq!(render(&[], false), expected);
        assert_eq!(render(&[], true), expected);
    }

    // The plain layout is what scripts parse. It is allowed to gain colour, but
    // not to change shape.
    #[test]
    fn the_plain_layout_keeps_its_exact_shape() {
        let out = render(&[hole("double the input")], false);
        assert_eq!(
            out,
            "1 hole(s) in /tmp/crate\n\
             \n\
             [ 1] src/lib.rs:12  open  (todo, expression)\n\
             \x20    fn:   pub fn double(n: i64) -> i64\n\
             \x20    spec: double the input\n\
             \n\
             summary: 1 open, 0 pinned, 0 unresolvable\n"
        );
    }

    #[test]
    fn the_plain_layout_still_mentions_an_unresolvable_reason() {
        let mut h = hole("double the input");
        h.unresolvable = Some("no trustworthy byte range".to_string());
        let out = render(&[h], false);
        assert!(out.contains("UNRESOLVABLE"));
        assert!(out.contains("     why:  no trustworthy byte range\n"));
    }

    #[test]
    fn pretty_groups_holes_under_their_file() {
        let mut a = hole("first spec");
        a.line = 12;
        let mut b = hole("second spec");
        b.line = 40;
        let mut c = hole("third spec");
        c.line = 7;
        c.file = PathBuf::from("/tmp/crate/src/util.rs");

        let out = render(&[a, b, c], true);
        let lib = out.find("src/lib.rs").expect("the first file is named");
        let util = out.find("src/util.rs").expect("the second file is named");
        assert!(lib < util, "files appear in the order given");
        assert_eq!(
            out.matches("src/lib.rs").count(),
            1,
            "the path is stated once per file, not once per hole"
        );
        assert_eq!(out.matches("src/util.rs").count(), 1);
    }

    #[test]
    fn pretty_does_not_repeat_a_path_for_each_hole() {
        let mut holes: Vec<Hole> = (1..=5)
            .map(|i| {
                let mut h = hole("a spec");
                h.line = i * 3;
                h
            })
            .collect();
        holes.sort_by_key(|h| h.line);
        let out = render(&holes, true);
        assert_eq!(out.matches("src/lib.rs").count(), 1);
    }

    #[test]
    fn pretty_names_a_pinned_hole_and_an_unresolvable_one_differently() {
        let mut pinned = hole("hand written");
        pinned.pinned = true;
        let mut broken = hole("cannot locate");
        broken.unresolvable = Some("no trustworthy byte range".to_string());

        let out = render(&[pinned, broken], true);
        assert!(out.contains("pinned"));
        assert!(out.contains("unresolvable"));
        assert!(out.contains("no trustworthy byte range"));
        assert!(out.contains("1 pinned hole(s)"), "the caveat is repeated");
    }

    // The two flags are set by independent checks, so a hole can carry both. The
    // plain layout's `if/else if` shows only one of them; the block layout must
    // not quietly drop the other.
    #[test]
    fn pretty_reports_a_hole_that_is_both_pinned_and_unresolvable() {
        let mut both = hole("hand written but unlocatable");
        both.pinned = true;
        both.unresolvable = Some("no trustworthy byte range".to_string());

        let out = render(&[both], true);
        assert!(
            out.contains("pinned+unresolved"),
            "both facts belong in the status column, got:\n{out}"
        );
        assert!(out.contains("no trustworthy byte range"));
    }

    #[test]
    fn pretty_wraps_a_long_spec_instead_of_truncating_it() {
        let long = "scale to fit within 1024px on the longest side, keep the aspect ratio, \
                    never upscale, and round to whole pixels";
        let out = render(&[hole(long)], true);
        assert!(
            out.contains("round to whole pixels"),
            "the default layout truncates, but the block layout is for reading"
        );
        assert!(!out.contains('…'), "nothing should be elided");
    }

    #[test]
    fn pretty_indents_wrapped_spec_lines_under_the_first() {
        // Narrow enough that this spec cannot fit on one line, so there is a
        // continuation to check.
        let narrow = RenderOptions {
            pretty: true,
            color: false,
            wrap_width: 60,
            spec_width: 60,
        };
        let long = "scale to fit within 1024px on the longest side, keep the aspect ratio, \
                    never upscale, and round to whole pixels";
        let out = render_list(&[hole(long)], Path::new("/tmp/crate"), &narrow);

        let spec_line = out
            .lines()
            .find(|l| l.contains("spec  scale to fit"))
            .unwrap_or_else(|| panic!("no spec line in:\n{out}"));
        let text_column = spec_line.find("scale").expect("the spec starts somewhere");

        // Every following line that carries more spec text must begin in that
        // same column, which is what makes a wrapped block readable.
        let continuations: Vec<&str> = out
            .lines()
            .skip_while(|l| !std::ptr::eq(*l, spec_line))
            .skip(1)
            .filter(|l| !l.trim().is_empty())
            .collect();
        assert!(
            !continuations.is_empty(),
            "the spec should have wrapped at this width:\n{out}"
        );
        for line in continuations {
            let start = line.len() - line.trim_start().len();
            assert_eq!(
                start, text_column,
                "continuation {line:?} does not align with the spec text:\n{out}"
            );
        }
    }

    #[test]
    fn pretty_respects_the_wrap_width() {
        let long = "one two three four five six seven eight nine ten eleven twelve thirteen";
        let narrow = RenderOptions {
            pretty: true,
            color: false,
            wrap_width: 40,
            spec_width: 60,
        };
        let out = render_list(&[hole(long)], Path::new("/tmp/crate"), &narrow);
        for line in out.lines() {
            assert!(
                text_width(line) <= 40,
                "line is {} cells wide: {line:?}",
                text_width(line)
            );
        }
    }

    #[test]
    fn pretty_keeps_a_word_too_long_for_the_line_from_overflowing_it() {
        let narrow = RenderOptions {
            pretty: true,
            color: false,
            wrap_width: 40,
            spec_width: 60,
        };
        // A path with no spaces has no word boundary to wrap at.
        let long = "a/very/long/path/that/cannot/be/broken/on/whitespace/at/all";
        let out = render_list(&[hole(long)], Path::new("/tmp/crate"), &narrow);
        for line in out.lines() {
            assert!(
                text_width(line) <= 40,
                "line overflows: {line:?} ({} cells)",
                text_width(line)
            );
        }
    }

    #[test]
    fn pretty_aligns_a_multi_byte_spec_by_display_width() {
        // CJK characters occupy two cells, so a column width counted in `char`s
        // would leave every line below them misaligned.
        let mut wide = hole("把图片缩放到最长边不超过 1024 像素，保持宽高比");
        wide.fn_sig = "pub fn resize(img: &Image) -> Image".to_string();
        let out = render(&[wide], true);
        for line in out.lines() {
            assert!(
                text_width(line) <= 100,
                "line is {} cells wide: {line:?}",
                text_width(line)
            );
        }
    }

    #[test]
    fn a_cjk_character_counts_as_two_cells() {
        assert_eq!(text_width("a"), 1);
        assert_eq!(text_width("中"), 2);
        assert_eq!(text_width("中a"), 3);
        // Zero-width formatting characters must not consume a column.
        assert_eq!(text_width("a\u{0301}"), 1);
    }

    #[test]
    fn pretty_shows_the_impl_header_for_a_method() {
        let mut method = hole("bump the counter");
        method.impl_ctx = Some("impl Counter".to_string());
        let out = render(&[method], true);
        assert!(
            out.contains("impl Counter"),
            "the receiver type matters:\n{out}"
        );
    }

    #[test]
    fn pretty_notes_a_hole_hidden_in_a_macro_body() {
        let mut hidden = hole("double each element");
        hidden.position = HolePosition::MacroBody;
        let out = render(&[hidden], true);
        assert!(
            out.contains("inside a `todo!` body"),
            "a macro-body hole explains odd probe results, so it is worth saying:\n{out}"
        );
    }

    #[test]
    fn pretty_stays_quiet_about_the_ordinary_position() {
        let out = render(&[hole("double the input")], true);
        assert!(
            !out.contains("note"),
            "the common case should not spend a line on itself:\n{out}"
        );
    }

    #[test]
    fn pretty_agrees_with_the_count_for_a_single_hole() {
        let out = render(&[hole("double the input")], true);
        assert!(out.contains("1 hole:"), "not `1 hole(s)`, got:\n{out}");
    }

    #[test]
    fn pretty_leads_with_the_crate_name() {
        let out = render(&[hole("double the input")], true);
        let first = out.lines().next().expect("a header line");
        assert!(first.starts_with("crate — "), "got: {first:?}");
    }

    #[test]
    fn color_is_off_when_never_and_on_when_always() {
        let tty = Env {
            is_tty: true,
            ..Env::default()
        };
        assert!(!color_enabled(ColorMode::Never, &tty));
        assert!(color_enabled(ColorMode::Always, &tty));

        // `always` wins over a pipe, which is the point of asking for it.
        let piped = Env::default();
        assert!(color_enabled(ColorMode::Always, &piped));
        assert!(!color_enabled(ColorMode::Auto, &piped));
    }

    #[test]
    fn auto_color_needs_a_terminal_and_no_opt_out() {
        let plain_tty = Env {
            is_tty: true,
            ..Env::default()
        };
        assert!(color_enabled(ColorMode::Auto, &plain_tty));

        let opted_out = Env {
            is_tty: true,
            no_color: Some("1".to_string()),
            ..Env::default()
        };
        assert!(
            !color_enabled(ColorMode::Auto, &opted_out),
            "NO_COLOR is a request, and Auto has not been told otherwise"
        );

        let dumb = Env {
            is_tty: true,
            term: Some("dumb".to_string()),
            ..Env::default()
        };
        assert!(!color_enabled(ColorMode::Auto, &dumb));
    }

    // An explicit flag is a stronger signal than an environment default.
    #[test]
    fn an_explicit_always_overrides_no_color() {
        let opted_out = Env {
            is_tty: true,
            no_color: Some("1".to_string()),
            ..Env::default()
        };
        assert!(color_enabled(ColorMode::Always, &opted_out));
    }

    #[test]
    fn color_adds_escapes_without_changing_the_text() {
        let colored = render_list(
            &[hole("double the input")],
            Path::new("/tmp/crate"),
            &RenderOptions {
                pretty: true,
                color: true,
                wrap_width: 100,
                spec_width: 60,
            },
        );
        assert!(colored.contains("\x1b["), "colour should escape");
        assert!(colored.contains("double the input"));
        assert!(colored.contains("open"));
    }

    #[test]
    fn the_plain_layout_is_never_colored() {
        // It is the format scripts read; the function cannot even see the
        // option, so this is a guard against someone wiring it up later.
        let out = render(&[hole("double the input")], false);
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn width_comes_from_columns_when_it_is_sane() {
        let env = |c: &str| Env {
            columns: Some(c.to_string()),
            ..Env::default()
        };
        assert_eq!(render_width(&env("120")), 120);
        assert_eq!(render_width(&env(" 120 ")), 120, "trimmed");
        assert_eq!(render_width(&env("not a number")), DEFAULT_WIDTH);
        assert_eq!(
            render_width(&env("0")),
            DEFAULT_WIDTH,
            "zero is not a width"
        );
        // Clamped, so a stale COLUMNS cannot produce unreadable output.
        assert_eq!(render_width(&env("5")), MIN_WIDTH);
        assert_eq!(render_width(&env("10000")), MAX_WIDTH);
        assert_eq!(render_width(&Env::default()), DEFAULT_WIDTH);
    }

    #[test]
    fn wrap_returns_one_empty_line_for_empty_text() {
        // So the caller emits a label rather than nothing at all.
        assert_eq!(wrap("", 20), vec![String::new()]);
        assert_eq!(wrap("   ", 20), vec![String::new()]);
    }

    #[test]
    fn wrap_keeps_short_text_on_one_line() {
        assert_eq!(wrap("hello world", 40), vec!["hello world".to_string()]);
    }

    #[test]
    fn wrap_breaks_at_whitespace_and_drops_the_space() {
        let lines = wrap("one two three four", 8);
        assert_eq!(
            lines,
            vec![
                "one two".to_string(),
                "three".to_string(),
                "four".to_string()
            ]
        );
    }

    #[test]
    fn a_hole_in_item_position_does_not_print_a_blank_signature_line() {
        let mut item = hole("add a helper");
        item.fn_sig = String::new();
        item.position = HolePosition::Item;
        let out = render(&[item], true);
        assert!(out.contains("add a helper"));
        assert!(
            !out.lines().any(|l| !l.is_empty() && l.trim().is_empty()),
            "no line should be only padding:\n{out}"
        );
    }
}
