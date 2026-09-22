use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::util;

/// Where in the syntax tree a hole sits. This is a hint for the type probe:
/// a hole in statement position is known to be `()` even when rustc's type
/// inference cannot say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HolePosition {
    /// The hole is a whole statement: `todo!("spec: ...");`
    Statement,
    /// The hole is an expression in some other position (tail, argument, ...).
    Expression,
    /// The hole is inside another macro's token body: `vec![todo!("spec: ...")]`
    MacroBody,
    /// The hole sits in item position.
    Item,
    /// Position could not be determined.
    Unknown,
}

impl HolePosition {
    pub fn as_str(self) -> &'static str {
        match self {
            HolePosition::Statement => "statement",
            HolePosition::Expression => "expression",
            HolePosition::MacroBody => "macro-body",
            HolePosition::Item => "item",
            HolePosition::Unknown => "unknown",
        }
    }
}

/// A single `todo!("spec: ...")` placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hole {
    /// Source file containing the hole.
    pub file: PathBuf,
    /// Byte offset of the `t` in `todo!`, i.e. the inclusive start of the hole.
    pub byte_start: usize,
    /// Byte offset one past the closing `)`, i.e. the exclusive end.
    pub byte_end: usize,
    /// The spec text, with the `spec:` prefix and surrounding whitespace removed.
    pub spec: String,
    /// Source text of the enclosing function's signature.
    pub fn_sig: String,
    /// Name of the enclosing function, for reporting.
    pub fn_name: String,
    /// Enclosing `impl`/`trait` header, when the hole is in a method body.
    pub impl_ctx: Option<String>,
    /// 1-based line of the hole.
    pub line: usize,
    /// 1-based column of the hole, in characters.
    pub column: usize,
    /// Which macro was used: `todo` or `unimplemented`.
    pub macro_name: String,
    /// Syntactic position, used as a probe hint.
    pub position: HolePosition,
    /// True when the hole is marked `// hole:pinned` (or the fn carries
    /// `#[hole::pin]`). Pinned holes are never regenerated.
    pub pinned: bool,
    /// Set when the hole was found but its byte range could not be trusted.
    /// Such holes are reported to the user and never acted upon.
    pub unresolvable: Option<String>,
}

pub type HoleIndex = (PathBuf, usize, usize);

impl Hole {
    /// Every hole in the crate, sorted by file then position.
    pub fn list_holes(root: &Path) -> Result<Vec<Hole>> {
        let mut all: Vec<Hole> = Vec::new();
        for file in util::rust_files(root)? {
            // A file that does not parse is reported, not fatal: one broken file
            // should not hide every hole in the crate.
            match Self::list_holes_one_file(&file) {
                Ok(mut hs) => all.append(&mut hs),
                Err(e) => eprintln!(
                    "warning: skipping {}: {e}",
                    util::display_rel_path(root, &file)
                ),
            }
        }
        all.sort_by(|a, b| a.file.cmp(&b.file).then(a.byte_start.cmp(&b.byte_start)));
        Ok(all)
    }

    /// Extract every numbered hole from one source file.
    pub fn list_holes_one_file(path: &Path) -> Result<Vec<Hole>> {
        let src = std::fs::read_to_string(&path)?;
        let ast = syn::parse_file(&src)
            .with_context(|| format!("error: failed to parse {}", path.display()))?;
        let mut finder = Finder::new(path, &src);
        finder.visit_file(&ast);

        let mut holes = finder.holes;
        holes.sort_by_key(|h| (h.byte_start, h.byte_end));
        holes.dedup_by_key(|h| (h.byte_start, h.byte_end));
        Ok(holes)
    }

    /// `path:line` identifier used by the CLI and the ledger.
    pub fn location(&self) -> String {
        return format!("{}:{}", self.file.display(), self.line);
    }

    /// Short single-line form of the spec, for `list` output.
    pub fn spec_summary(&self, max_chars: usize) -> String {
        let flat: String = self
            .spec
            .chars()
            .map(|c| if c == '\n' { ' ' } else { c })
            .collect();
        if flat.chars().count() <= max_chars {
            return flat;
        }
        let truncated: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
        return format!("{truncated}…");
    }

    /// The exact source text of the hole.
    pub fn source_slice<'a>(&self, src: &'a str) -> Option<&'a str> {
        src.get(self.byte_start..self.byte_end)
    }

    pub fn hash_key(&self) -> String {
        let mut h = blake3::Hasher::new();
        h.update(b"cargo-hole/v1\0");                      // 换方案时递增，旧条目自动失配
        // L0 优先级：规约
        h.update(collapse_ws(&self.spec).as_bytes());      // 折叠空白
        fn collapse_ws(s: &str) -> String {
            s.split_whitespace().collect::<Vec<_>>().join(" ")
        }
        h.update(b"\0");
        // L1 优先级：局部上下文
        h.update(self.fn_sig.trim().as_bytes());           // 契约变了就必须重算
        h.update(b"\0");
        h.update(self.impl_ctx.as_deref().unwrap_or("").trim().as_bytes());
        h.update(b"\0");
        h.update(self.position.as_str().as_bytes());       // statement / expression 影响期望类型
        // L2 优先级：全局上下文（未完待续）
        // L3 优先级：环境（未完待续）
        h.finalize().to_hex()[..16].to_string()
    }

    pub fn index_key(&self) -> (PathBuf, usize, usize) {
        (self.file.clone(), self.byte_start, self.byte_end)
    }
}

const SPEC_PREFIX: &str = "spec:";
const PIN_MARKER: &str = "hole:pinned";

/// Enclosing-function context, tracked as a stack so that nested items (a
/// function inside a function) resolve to the innermost one.
#[derive(Clone)]
struct FnCtx {
    sig: String,
    name: String,
}

struct Finder<'a> {
    path: &'a Path,
    src: &'a str,
    fn_stack: Vec<FnCtx>,
    impl_stack: Vec<String>,
    position_stack: Vec<HolePosition>,
    /// True while inside an item carrying `#[hole::pin]` / `#[pin]`.
    pin_stack: Vec<bool>,
    holes: Vec<Hole>,
}

impl<'a> Finder<'a> {
    fn new(path: &'a Path, src: &'a str) -> Self {
        Self {
            path,
            src,
            fn_stack: Vec::new(),
            impl_stack: Vec::new(),
            position_stack: Vec::new(),
            pin_stack: Vec::new(),
            holes: Vec::new(),
        }
    }

    fn current_position(&self) -> HolePosition {
        self.position_stack
            .last()
            .copied()
            .unwrap_or(HolePosition::Unknown)
    }

    fn enclosing_fn(&self) -> Option<&FnCtx> {
        self.fn_stack.last()
    }

    /// Run `body` with `position` as the innermost position.
    fn with_position<F, R>(&mut self, position: HolePosition, body: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        self.position_stack.push(position);
        let out = body(self);
        self.position_stack.pop();
        out
    }

    /// Run `body` with the pin flag implied by `attrs` pushed.
    fn with_pin_attr<F, R>(&mut self, attrs: &[syn::Attribute], body: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        self.pin_stack.push(has_pin_attribute(attrs));
        let out = body(self);
        self.pin_stack.pop();
        out
    }

    /// Record a hole if `mac` is a `todo!`/`unimplemented!` carrying a spec.
    ///
    /// `range` is the trusted byte range of the whole invocation when the
    /// caller already knows it (the token walk does); otherwise it is derived
    /// from spans here.
    fn consider_macro(&mut self, mac: &syn::Macro, range: Option<(usize, usize)>) {
        let macro_name = mac
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();

        // The token body is searched regardless of whether *this* macro is a
        // hole: `todo!("not a spec", todo!("spec: x"))` still contains a hole,
        // and a nested `todo!` inside a spec string is a single Literal token
        // so it can never be mistaken for a second hole.
        self.descend_tokens(&mac.tokens);

        if macro_name != "todo" && macro_name != "unimplemented" {
            return;
        }

        // Only the parenthesised form is a hole, matching the documented
        // convention. Both the visitor path and the token walk enforce this, so
        // the same source always yields the same holes.
        if !matches!(mac.delimiter, syn::MacroDelimiter::Paren(_)) {
            return;
        }

        let Some(spec) = spec_from_macro(mac) else {
            return;
        };

        let position = self.current_position();
        let ctx = self.enclosing_fn().cloned();

        let (byte_start, byte_end, unresolvable) = match range
            .or_else(|| macro_range(self.src, mac))
        {
            Some((start, end)) if end > start && end <= self.src.len() => (start, end, None),
            _ => {
                // Fall back to the path span start so we can still report the
                // hole's location, but mark it unusable.
                let start = mac.path.span().byte_range().start;
                let start = start.min(self.src.len());
                (
                    start,
                    start,
                    Some("could not determine a trustworthy byte range for this hole".to_string()),
                )
            }
        };

        let pinned = preceding_pin_marker(self.src, byte_start)
            || self.pin_stack.last().copied().unwrap_or(false);

        self.holes.push(Hole {
            file: self.path.to_path_buf(),
            byte_start,
            byte_end,
            spec,
            fn_sig: ctx.as_ref().map(|c| c.sig.clone()).unwrap_or_default(),
            fn_name: ctx.as_ref().map(|c| c.name.clone()).unwrap_or_default(),
            impl_ctx: self.impl_stack.last().cloned(),
            line: util::line_of(self.src, byte_start),
            column: util::column_of(self.src, byte_start),
            macro_name,
            position,
            pinned,
            unresolvable,
        });
    }

    /// Recursively search a macro token body for nested holes.
    ///
    /// `syn` hands macro bodies to us as raw tokens and never visits inside
    /// them, so this walk is the only way to see `vec![todo!("spec: ...")]`.
    fn descend_tokens(&mut self, tokens: &TokenStream) {
        self.descend_tokens_inner(tokens, 0);
    }

    fn descend_tokens_inner(&mut self, tokens: &TokenStream, depth: usize) {
        // Guard against pathological nesting; real code never goes this deep.
        if depth > 32 {
            return;
        }
        let trees: Vec<TokenTree> = tokens.clone().into_iter().collect();
        let mut i = 0;
        while i < trees.len() {
            let TokenTree::Ident(ident) = &trees[i] else {
                if let TokenTree::Group(g) = &trees[i] {
                    self.descend_tokens_inner(&g.stream(), depth + 1);
                }
                i += 1;
                continue;
            };

            let name = ident.to_string();
            let is_candidate = name == "todo" || name == "unimplemented";
            if !is_candidate {
                i += 1;
                continue;
            }

            // Expect `!` then a delimited group.
            let Some(TokenTree::Punct(punct)) = trees.get(i + 1) else {
                i += 1;
                continue;
            };
            if punct.as_char() != '!' {
                i += 1;
                continue;
            }
            let Some(TokenTree::Group(group)) = trees.get(i + 2) else {
                i += 1;
                continue;
            };
            if group.delimiter() != Delimiter::Parenthesis {
                i += 1;
                continue;
            }

            let range = token_range(ident, group);
            // Build a `syn::Macro` so that spec parsing is shared with the
            // visitor path. The span here is synthetic, which is fine because
            // we already hold the trusted byte range from the tokens.
            let path: syn::Path = syn::parse_quote!(#ident);
            let mac = syn::Macro {
                path,
                bang_token: syn::Token![!](punct.span()),
                delimiter: syn::MacroDelimiter::Paren(syn::token::Paren(group.span_open())),
                tokens: group.stream(),
            };

            self.position_stack.push(HolePosition::MacroBody);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.consider_macro(&mac, range)
            }));
            self.position_stack.pop();
            // Advance past `ident ! group` in every case, including the error
            // path, so the walk always makes progress.
            i += 3;
            if result.is_err() {
                // A synthetic-span macro should never panic, but a hole inside
                // a macro body is exactly where spans are least trustworthy, so
                // fail soft rather than taking down the whole scan.
                continue;
            }

            self.descend_tokens_inner(&group.stream(), depth + 1);
        }
    }
}

impl<'a, 'ast> Visit<'ast> for Finder<'a> {
    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        self.fn_stack.push(FnCtx {
            sig: signature_text(self.src, &node.sig),
            name: node.sig.ident.to_string(),
        });
        self.pin_stack.push(has_pin_attribute(&node.attrs));
        syn::visit::visit_item_fn(self, node);
        self.pin_stack.pop();
        self.fn_stack.pop();
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        self.fn_stack.push(FnCtx {
            sig: signature_text(self.src, &node.sig),
            name: node.sig.ident.to_string(),
        });
        self.pin_stack.push(has_pin_attribute(&node.attrs));
        syn::visit::visit_impl_item_fn(self, node);
        self.pin_stack.pop();
        self.fn_stack.pop();
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        self.fn_stack.push(FnCtx {
            sig: signature_text(self.src, &node.sig),
            name: node.sig.ident.to_string(),
        });
        self.pin_stack.push(has_pin_attribute(&node.attrs));
        syn::visit::visit_trait_item_fn(self, node);
        self.pin_stack.pop();
        self.fn_stack.pop();
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        self.impl_stack.push(render_tokens(&node.self_ty));
        self.pin_stack.push(has_pin_attribute(&node.attrs));
        syn::visit::visit_item_impl(self, node);
        self.pin_stack.pop();
        self.impl_stack.pop();
    }

    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        self.impl_stack.push(format!("trait {}", node.ident));
        self.pin_stack.push(has_pin_attribute(&node.attrs));
        syn::visit::visit_item_trait(self, node);
        self.pin_stack.pop();
        self.impl_stack.pop();
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        // `visit_macro` is reached from every macro-bearing position
        // (expr, stmt, item, impl item, trait item), so this is the single
        // place holes are recognised -- no double counting.
        self.consider_macro(node, None);
    }

    fn visit_stmt_macro(&mut self, node: &'ast syn::StmtMacro) {
        self.with_position(HolePosition::Statement, |me| {
            me.with_pin_attr(&node.attrs, |me| syn::visit::visit_stmt_macro(me, node))
        });
    }

    fn visit_expr_macro(&mut self, node: &'ast syn::ExprMacro) {
        self.with_position(HolePosition::Expression, |me| {
            me.with_pin_attr(&node.attrs, |me| syn::visit::visit_expr_macro(me, node))
        });
    }

    fn visit_item_macro(&mut self, node: &'ast syn::ItemMacro) {
        self.with_position(HolePosition::Item, |me| {
            me.with_pin_attr(&node.attrs, |me| syn::visit::visit_item_macro(me, node))
        });
    }

    fn visit_impl_item_macro(&mut self, node: &'ast syn::ImplItemMacro) {
        let attrs = node.attrs.clone();
        self.with_pin_attr(&attrs, |me| syn::visit::visit_impl_item_macro(me, node));
    }

    fn visit_trait_item_macro(&mut self, node: &'ast syn::TraitItemMacro) {
        let attrs = node.attrs.clone();
        self.with_pin_attr(&attrs, |me| syn::visit::visit_trait_item_macro(me, node));
    }
}

/// Byte range of `todo!("...")` taken from its tokens: the ident's start to the
/// closing delimiter's end. Verified against the fixtures to be exact.
fn token_range(ident: &proc_macro2::Ident, group: &proc_macro2::Group) -> Option<(usize, usize)> {
    let start = ident.span().byte_range().start;
    let end = group.span_close().byte_range().end;
    if end > start {
        Some((start, end))
    } else {
        None
    }
}

/// Trusted byte range of a whole macro invocation.
fn macro_range(src: &str, mac: &syn::Macro) -> Option<(usize, usize)> {
    let bytes = src.as_bytes();

    // Preferred: the exact span of the whole invocation.
    let r = mac.span().byte_range();
    if r.end > r.start
        && r.end <= src.len()
        && src.is_char_boundary(r.start)
        && src.is_char_boundary(r.end)
    {
        let slice = &src[r.start..r.end];
        // Sanity check: the slice must begin with the macro's own name. This
        // rejects the degenerate `0..0` span and any stale span.
        let name = mac
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();
        if !name.is_empty() && slice.starts_with(&name) {
            return Some((r.start, r.end));
        }
    }

    // Fallback: locate the body by scanning from the path span.
    let path_start = mac.path.span().byte_range().start;
    if path_start >= src.len() {
        return None;
    }
    let open = util::macro_body_open(bytes, path_start)?;
    let close = util::matching_close(bytes, open)?;
    Some((path_start, close + 1))
}

/// Parse the `spec:` payload out of a `todo!`-like macro.
///
/// Returns `None` when the invocation is not a cargo-hole spec: no argument, no
/// literal, no `spec:` prefix, an empty spec, or an argument list we do not
/// understand (anything beyond a single string and an optional trailing comma).
fn spec_from_macro(mac: &syn::Macro) -> Option<String> {
    let trees: Vec<TokenTree> = mac.tokens.clone().into_iter().collect();
    let first = trees.first()?;
    let TokenTree::Literal(lit) = first else {
        return None;
    };
    let parsed = syn::Lit::new(lit.clone());
    let syn::Lit::Str(s) = parsed else {
        return None;
    };

    // Anything after the string must be nothing, or a single trailing comma.
    match &trees[1..] {
        [] => {}
        [TokenTree::Punct(p)] if p.as_char() == ',' => {}
        _ => return None,
    }

    let value = s.value();
    let rest = value.trim_start().strip_prefix(SPEC_PREFIX)?;
    let spec = rest.trim();
    if spec.is_empty() {
        return None;
    }
    Some(spec.to_string())
}

/// True when the contiguous comment block immediately above `hole_start`
/// carries the `hole:pinned` marker.
fn preceding_pin_marker(src: &str, hole_start: usize) -> bool {
    let before = &src[..hole_start.min(src.len())];
    let mut end = before.len();
    loop {
        while end > 0 && before.as_bytes()[end - 1].is_ascii_whitespace() {
            end -= 1;
        }
        if end == 0 {
            return false;
        }
        let line_start = before[..end].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let line = before[line_start..end].trim();
        match line.strip_prefix("//") {
            Some(rest) => {
                if rest.contains(PIN_MARKER) {
                    return true;
                }
                end = line_start;
            }
            None => return false,
        }
    }
}

/// True when an item's attribute list contains a pin marker.
///
/// `#[hole::pin]` and `#[pin]` are both accepted. Pinned holes are listed
/// prominently by the CLI and never regenerated.
pub fn has_pin_attribute(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        let mut segs = a.path().segments.iter();
        match segs.next() {
            Some(first) if first.ident == "pin" => segs.next().is_none(),
            Some(first) if first.ident == "hole" => {
                return matches!(segs.next(), Some(s) if s.ident == "pin");
            }
            _ => false,
        }
    })
}

/// Source text of a function signature.
///
/// Slicing the source is preferred over re-rendering the token stream: `syn`
/// prints a `where` clause as part of `Generics`, which lands in the wrong
/// place for display (`fn f<T> where T: X(...)`). The fallback only runs when
/// the span is unusable.
fn signature_text(src: &str, sig: &syn::Signature) -> String {
    let r = sig.span().byte_range();
    if r.end > r.start
        && r.end <= src.len()
        && src.is_char_boundary(r.start)
        && src.is_char_boundary(r.end)
    {
        let slice = &src[r.start..r.end];
        if slice.starts_with("fn")
            || slice.starts_with("pub")
            || slice.starts_with("const")
            || slice.starts_with("async")
            || slice.starts_with("unsafe")
            || slice.starts_with("extern")
        {
            return collapse_whitespace(slice);
        }
    }
    fallback_signature_text(sig)
}

/// Assemble a signature from tokens, placing the `where` clause after the
/// return type where it belongs.
fn fallback_signature_text(sig: &syn::Signature) -> String {
    let mut s = String::new();
    if sig.constness.is_some() {
        s.push_str("const ");
    }
    if sig.asyncness.is_some() {
        s.push_str("async ");
    }
    if sig.unsafety.is_some() {
        s.push_str("unsafe ");
    }
    s.push_str("fn ");
    s.push_str(&sig.ident.to_string());
    if !sig.generics.params.is_empty() {
        let params = sig
            .generics
            .params
            .iter()
            .map(render_tokens)
            .collect::<Vec<_>>();
        s.push('<');
        s.push_str(&params.join(", "));
        s.push('>');
    }
    let args = sig.inputs.iter().map(render_tokens).collect::<Vec<_>>();
    s.push('(');
    s.push_str(&args.join(", "));
    s.push(')');
    match &sig.output {
        syn::ReturnType::Default => {}
        syn::ReturnType::Type(_, ty) => {
            s.push_str(" -> ");
            s.push_str(&render_tokens(ty));
        }
    }
    if let Some(wc) = &sig.generics.where_clause {
        s.push(' ');
        s.push_str(&render_tokens(wc));
    }
    s
}

fn render_tokens<T: ToTokens>(node: &T) -> String {
    collapse_whitespace(&node.to_token_stream().to_string())
}

/// Collapse runs of whitespace to single spaces and tidy spacing around
/// punctuation that token rendering leaves ragged.
fn collapse_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out.trim().to_string()
}
