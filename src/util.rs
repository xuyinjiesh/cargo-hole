use std::path::{Path, PathBuf};

use anyhow::Result;

pub fn display_rel_path(root: &Path, file: &Path) -> String {
    match file.strip_prefix(root) {
        // `root` relative to itself is empty, which would print as nothing at
        // all; report it as "." the way a shell would.
        Ok(rel) if rel.as_os_str().is_empty() => ".".to_string(),
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => file.to_string_lossy().replace('\\', "/"),
    }
}

pub fn line_of(src: &str, byte: usize) -> usize {
    let byte = byte.min(src.len());
    1 + src.as_bytes()[..byte]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
}

/// 1-based column number (in characters) of a byte offset.
pub fn column_of(src: &str, byte: usize) -> usize {
    let byte = byte.min(src.len());
    let line_start = src.as_bytes()[..byte]
        .iter()
        .rposition(|&b| b == b'\n')
        .map(|p| p + 1)
        .unwrap_or(0);
    src[line_start..byte].chars().count() + 1
}


pub fn rust_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(root, &mut out, 0)?;
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) -> Result<()> {
    // A crude but effective guard against symlink cycles.
    if depth > 32 {
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        // Unreadable directories are skipped rather than fatal.
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            if name == "target" || name == "node_modules" {
                continue;
            }
            walk(&path, out, depth + 1)?;
        } else if ft.is_file() && path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delim {
    pub open: u8,
    pub close: u8,
}

impl Delim {
    pub const PAREN: Delim = Delim {
        open: b'(',
        close: b')',
    };
    pub const BRACKET: Delim = Delim {
        open: b'[',
        close: b']',
    };
    pub const BRACE: Delim = Delim {
        open: b'{',
        close: b'}',
    };

    pub fn from_open_byte(b: u8) -> Option<Delim> {
        match b {
            b'(' => Some(Delim::PAREN),
            b'[' => Some(Delim::BRACKET),
            b'{' => Some(Delim::BRACE),
            _ => None,
        }
    }
}

/// Find the byte index of the matching close delimiter for the opener at
/// `open_idx`.
///
/// Returns the index *of the closing byte* (not one past it). Returns `None`
/// if the source is truncated, if `open_idx` does not point at an opener, or if
/// the nesting does not balance.
///
/// The scan is aware of:
/// - line comments (`// ...`) and nested block comments (`/* /* */ */`)
/// - normal strings with escapes (`"a\"b"`)
/// - raw strings with any number of `#` (`r#"..."#`, `r##"..."##`)
/// - byte strings and byte chars (`b"..."`, `b'x'`) and raw byte strings (`br#"..."#`)
/// - char literals (`'x'`, `'\n'`, `'\''`)
/// - lifetimes (`'a`), which are NOT char literals and must not derail the scan
pub fn matching_close(src: &[u8], open_idx: usize) -> Option<usize> {
    let open = *src.get(open_idx)?;
    let delim = Delim::from_open_byte(open)?;
    let mut depth = 0usize;
    let mut i = open_idx;

    while i < src.len() {
        let b = src[i];

        // Comments and literals can only start where a token can start, and we
        // are always at a token boundary here because we consume them wholesale.
        if b == b'/' && src.get(i + 1) == Some(&b'/') {
            i = skip_line_comment(src, i);
            continue;
        }
        if b == b'/' && src.get(i + 1) == Some(&b'*') {
            i = skip_block_comment(src, i)?;
            continue;
        }

        // Raw strings: r"..." / r#"..."# / br#"..."# / rb"..." etc.
        if let Some(next) = raw_string_end(src, i) {
            i = next;
            continue;
        }

        if b == b'"' {
            i = skip_normal_string(src, i, b'"')?;
            continue;
        }

        // A char literal vs a lifetime. `'a'` is a char; `'a` is a lifetime.
        if b == b'\'' {
            if let Some(next) = skip_char_or_lifetime(src, i) {
                i = next;
                continue;
            }
            i += 1;
            continue;
        }

        if b == delim.open {
            depth += 1;
        } else if b == delim.close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// Scan forward from the `!` of a macro to the opening delimiter of its body.
///
/// `syn` hands us the macro path span, and the `!` and `(` normally sit
/// immediately after it, but we scan rather than assume so that whitespace and
/// comments between path, bang and delimiter are tolerated. Returns the byte
/// index of the opening delimiter.
pub fn macro_body_open(src: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    // Bound the search: a path and its bang should be adjacent in practice.
    let limit = (from + 64).min(src.len());
    while i < limit {
        match src[i] {
            b'(' | b'[' | b'{' => return Some(i),
            b'/' if src.get(i + 1) == Some(&b'/') => i = skip_line_comment(src, i),
            b'/' if src.get(i + 1) == Some(&b'*') => i = skip_block_comment(src, i)?,
            b'"' => i = skip_normal_string(src, i, b'"')?,
            _ => i += 1,
        }
    }
    None
}

fn skip_line_comment(src: &[u8], start: usize) -> usize {
    let mut i = start;
    while i < src.len() && src[i] != b'\n' {
        i += 1;
    }
    i
}

/// Block comments nest in Rust, so track depth. Returns the index just past the
/// terminating `*/`, or `None` if unterminated.
fn skip_block_comment(src: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 2;
    let mut depth = 1usize;
    while i < src.len() {
        if src[i] == b'/' && src.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if src[i] == b'*' && src.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return Some(i);
            }
        } else {
            i += 1;
        }
    }
    None
}

/// Skip a normal (non-raw) string literal starting at `start`. Returns the
/// index just past the closing quote.
fn skip_normal_string(src: &[u8], start: usize, quote: u8) -> Option<usize> {
    let mut i = start + 1;
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b if b == quote => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// If a raw/byte string literal starts at `start`, return the index just past
/// its terminator. Recognises `r"..."`, `r#"..."#`, `b"..."`, `br#"..."#`,
/// `rb"..."` and any hash count.
fn raw_string_end(src: &[u8], start: usize) -> Option<usize> {
    let mut i = start;
    let is_raw;

    // Optional `b`/`r` prefixes in either order.
    match src.get(i) {
        Some(b'r') => {
            is_raw = true;
            i += 1;
        }
        Some(b'b') => {
            i += 1;
            if src.get(i) == Some(&b'r') {
                is_raw = true;
                i += 1;
            } else if src.get(i) == Some(&b'"') {
                // Plain byte string: not raw, but it is a literal we must skip.
                return skip_normal_string(src, i, b'"');
            } else {
                return None;
            }
        }
        _ => return None,
    }

    if !is_raw {
        return None;
    }

    let mut hashes = 0usize;
    while src.get(i) == Some(&b'#') {
        hashes += 1;
        i += 1;
    }
    if src.get(i) != Some(&b'"') {
        return None;
    }
    i += 1;

    // Find `"` followed by exactly `hashes` `#` characters.
    while i < src.len() {
        if src[i] == b'"' {
            let after = i + 1;
            if src.len() >= after + hashes && src[after..after + hashes].iter().all(|&c| c == b'#')
            {
                return Some(after + hashes);
            }
        }
        i += 1;
    }
    None
}

/// Distinguish a char literal from a lifetime and skip it if it is a char.
/// Returns the index just past the literal, or `None` if this is a lifetime
/// (i.e. the caller should just advance one byte).
fn skip_char_or_lifetime(src: &[u8], start: usize) -> Option<usize> {
    let b1 = *src.get(start + 1)?;
    if b1 == b'\\' {
        // Escaped char: `'\n'`, `'\''`, `'\u{1F600}'`.
        let mut i = start + 2;
        while i < src.len() {
            if src[i] == b'\'' {
                return Some(i + 1);
            }
            if src[i] == b'\n' {
                return None;
            }
            i += 1;
        }
        return None;
    }
    // `'a'` is a char literal; `'a` or `'ab` is a lifetime or a char.
    if src.get(start + 2) == Some(&b'\'') {
        return Some(start + 3);
    }
    None
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    return format!("{}…", &s[..end]);
}
