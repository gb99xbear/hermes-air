//! 1-to-1 Transpilation of Python Hermes tools/fuzzy_match.py into Rust.
//!
//! An ordered chain of increasingly permissive strategies lets
//! whitespace, indentation, escaping and Unicode drift in tool-call arguments
//! still land on the intended region:
//!
//! (new_content, match_count, strategy, error) = fuzzy_find_and_replace(
//!     content, old_string, new_string, replace_all);

use std::collections::HashMap;

pub const IDENTICAL_STRINGS_ERROR: &str =
    "No edit was applied because old_string and new_string are identical. \
    Provide the existing text to replace in old_string and the changed \
    replacement text in new_string.";

pub type Span = (usize, usize);

// Unicode map matching UNICODE_MAP in tools/fuzzy_match.py exactly
pub const UNICODE_MAP: &[(&str, &str)] = &[
    ("\u{201c}", "\""), // smart double quotes
    ("\u{201d}", "\""),
    ("\u{2018}", "'"),  // smart single quotes
    ("\u{2019}", "'"),
    ("\u{2014}", "--"), // em/en dashes
    ("\u{2013}", "-"),
    ("\u{2026}", "..."), // ellipsis
    ("\u{00a0}", " "),  // non-breaking space
    ("\u{2212}", "-"),  // typographic minus (math/scientific docs)
    // Space-separator family (Zs)
    ("\u{2000}", " "),
    ("\u{2001}", " "),
    ("\u{2002}", " "),
    ("\u{2003}", " "),
    ("\u{2004}", " "),
    ("\u{2005}", " "),
    ("\u{2006}", " "),
    ("\u{2007}", " "),
    ("\u{2008}", " "),
    ("\u{2009}", " "),
    ("\u{200a}", " "),
    ("\u{202f}", " "),
    ("\u{205f}", " "),
    ("\u{3000}", " "),
];

pub fn unicode_normalize(text: &str) -> String {
    let mut result = text.to_string();
    for (char_str, repl) in UNICODE_MAP {
        result = result.replace(char_str, repl);
    }
    result
}

// ── SequenceMatcher (Python difflib parity) ────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchBlock {
    pub a: usize,
    pub b: usize,
    pub size: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpcodeTag {
    Equal,
    Replace,
    Delete,
    Insert,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opcode {
    pub tag: OpcodeTag,
    pub i1: usize,
    pub i2: usize,
    pub j1: usize,
    pub j2: usize,
}

#[derive(Debug, Clone)]
pub struct SequenceMatcher<'a> {
    pub a: &'a str,
    pub b: &'a str,
    pub a_chars: Vec<char>,
    pub b_chars: Vec<char>,
}

impl<'a> SequenceMatcher<'a> {
    pub fn new(a: &'a str, b: &'a str) -> Self {
        Self {
            a,
            b,
            a_chars: a.chars().collect(),
            b_chars: b.chars().collect(),
        }
    }

    pub fn find_longest_match(
        &self,
        alo: usize,
        ahi: usize,
        blo: usize,
        bhi: usize,
    ) -> MatchBlock {
        let mut besti = alo;
        let mut bestj = blo;
        let mut bestsize = 0;

        let mut j2len: HashMap<usize, usize> = HashMap::new();

        for i in alo..ahi {
            let mut newj2len: HashMap<usize, usize> = HashMap::new();
            for j in blo..bhi {
                if self.a_chars[i] == self.b_chars[j] {
                    let k = if j > blo {
                        j2len.get(&(j - 1)).copied().unwrap_or(0) + 1
                    } else {
                        1
                    };
                    newj2len.insert(j, k);
                    if k > bestsize {
                        besti = i + 1 - k;
                        bestj = j + 1 - k;
                        bestsize = k;
                    }
                }
            }
            j2len = newj2len;
        }

        while besti > alo
            && bestj > blo
            && self.a_chars[besti - 1] == self.b_chars[bestj - 1]
        {
            besti -= 1;
            bestj -= 1;
            bestsize += 1;
        }
        while besti + bestsize < ahi
            && bestj + bestsize < bhi
            && self.a_chars[besti + bestsize] == self.b_chars[bestj + bestsize]
        {
            bestsize += 1;
        }

        MatchBlock {
            a: besti,
            b: bestj,
            size: bestsize,
        }
    }

    pub fn get_matching_blocks(&self) -> Vec<MatchBlock> {
        let la = self.a_chars.len();
        let lb = self.b_chars.len();
        let mut queue: Vec<(usize, usize, usize, usize)> = vec![(0, la, 0, lb)];
        let mut matching_blocks: Vec<MatchBlock> = Vec::new();

        while let Some((alo, ahi, blo, bhi)) = queue.pop() {
            let m = self.find_longest_match(alo, ahi, blo, bhi);
            if m.size > 0 {
                matching_blocks.push(m.clone());
                if alo < m.a && blo < m.b {
                    queue.push((alo, m.a, blo, m.b));
                }
                if m.a + m.size < ahi && m.b + m.size < bhi {
                    queue.push((m.a + m.size, ahi, m.b + m.size, bhi));
                }
            }
        }

        matching_blocks.sort_by(|x, y| x.a.cmp(&y.a).then_with(|| x.b.cmp(&y.b)));

        let mut non_adjacent: Vec<MatchBlock> = Vec::new();
        let mut i1 = 0;
        let mut j1 = 0;
        let mut k1 = 0;

        for m in matching_blocks {
            if i1 + k1 == m.a && j1 + k1 == m.b {
                k1 += m.size;
            } else {
                if k1 > 0 {
                    non_adjacent.push(MatchBlock {
                        a: i1,
                        b: j1,
                        size: k1,
                    });
                }
                i1 = m.a;
                j1 = m.b;
                k1 = m.size;
            }
        }
        if k1 > 0 {
            non_adjacent.push(MatchBlock {
                a: i1,
                b: j1,
                size: k1,
            });
        }
        non_adjacent.push(MatchBlock {
            a: la,
            b: lb,
            size: 0,
        });

        non_adjacent
    }

    pub fn ratio(&self) -> f64 {
        let total = self.a_chars.len() + self.b_chars.len();
        if total == 0 {
            return 1.0;
        }
        let matches: usize = self
            .get_matching_blocks()
            .iter()
            .map(|mb| mb.size)
            .sum();
        (2.0 * matches as f64) / (total as f64)
    }

    pub fn get_opcodes(&self) -> Vec<Opcode> {
        let mut i = 0;
        let mut j = 0;
        let mut answer = Vec::new();

        for mb in self.get_matching_blocks() {
            let ai = mb.a;
            let bj = mb.b;
            let size = mb.size;

            let tag = if i < ai && j < bj {
                Some(OpcodeTag::Replace)
            } else if i < ai {
                Some(OpcodeTag::Delete)
            } else if j < bj {
                Some(OpcodeTag::Insert)
            } else {
                None
            };

            if let Some(t) = tag {
                answer.push(Opcode {
                    tag: t,
                    i1: i,
                    i2: ai,
                    j1: j,
                    j2: bj,
                });
            }

            i = ai + size;
            j = bj + size;

            if size > 0 {
                answer.push(Opcode {
                    tag: OpcodeTag::Equal,
                    i1: ai,
                    i2: i,
                    j1: bj,
                    j2: j,
                });
            }
        }

        answer
    }
}

// ── Position Helpers ───────────────────────────────────────────────────────

pub fn calculate_line_positions(
    content_lines: &[&str],
    start_line: usize,
    end_line: usize,
    content_length: usize,
) -> Span {
    let start_pos: usize = content_lines[..start_line]
        .iter()
        .map(|l| l.len() + 1)
        .sum();
    let sum_end: usize = content_lines[..end_line]
        .iter()
        .map(|l| l.len() + 1)
        .sum();
    let end_pos = if sum_end > 0 { sum_end - 1 } else { 0 };
    (start_pos, std::cmp::min(content_length, end_pos))
}

pub fn window_spans<F>(
    content: &str,
    content_lines: &[&str],
    n: usize,
    mut accept: F,
) -> Vec<Span>
where
    F: FnMut(usize) -> bool,
{
    if content_lines.len() < n {
        return Vec::new();
    }
    let mut spans = Vec::new();
    for i in 0..=(content_lines.len() - n) {
        if accept(i) {
            spans.push(calculate_line_positions(
                content_lines,
                i,
                i + n,
                content.len(),
            ));
        }
    }
    spans
}

pub fn match_transformed_lines<F>(content: &str, pattern: &str, transform: F) -> Vec<Span>
where
    F: Fn(&[&str]) -> Vec<String>,
{
    let content_lines: Vec<&str> = content.split('\n').collect();
    let pattern_lines: Vec<&str> = pattern.split('\n').collect();
    let pattern_norm = transform(&pattern_lines);
    let n = pattern_norm.len();
    if n == 0 {
        return Vec::new();
    }

    window_spans(content, &content_lines, n, |i| {
        transform(&content_lines[i..i + n]) == pattern_norm
    })
}

fn strip_boundary(lines: &[&str]) -> Vec<String> {
    let mut res: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    if !res.is_empty() {
        res[0] = res[0].trim().to_string();
        let last = res.len() - 1;
        if last > 0 {
            res[last] = res[last].trim().to_string();
        }
    }
    res
}

fn build_orig_to_norm_map(original: &str) -> Vec<usize> {
    let mut result = Vec::new();
    let mut norm_pos = 0;
    for c in original.chars() {
        result.push(norm_pos);
        let mut c_buf = [0u8; 4];
        let c_str = c.encode_utf8(&mut c_buf);
        let mut matched_repl: Option<&str> = None;
        for (u, repl) in UNICODE_MAP {
            if *u == c_str {
                matched_repl = Some(repl);
                break;
            }
        }
        if let Some(repl) = matched_repl {
            norm_pos += repl.chars().count();
        } else {
            norm_pos += 1;
        }
    }
    result.push(norm_pos);
    result
}

fn invert_norm_map(orig_to_norm: &[usize]) -> HashMap<usize, usize> {
    let mut inverted = HashMap::new();
    for (orig_pos, &norm_pos) in orig_to_norm[..orig_to_norm.len() - 1].iter().enumerate() {
        inverted.entry(norm_pos).or_insert(orig_pos);
    }
    inverted
}

fn norm_end_to_orig(orig_to_norm: &[usize], orig_start: usize, norm_end: usize) -> usize {
    let orig_len = orig_to_norm.len() - 1;
    let mut orig_end = orig_start;
    while orig_end < orig_len && orig_to_norm[orig_end] < norm_end {
        orig_end += 1;
    }
    orig_end
}

fn map_positions_norm_to_orig(
    orig_to_norm: &[usize],
    norm_matches: &[Span],
    orig_chars: &[char],
) -> Vec<Span> {
    let norm_to_orig_start = invert_norm_map(orig_to_norm);
    let mut results = Vec::new();
    for &(norm_start, norm_end) in norm_matches {
        let orig_start_char = match norm_to_orig_start.get(&norm_start) {
            Some(&pos) => pos,
            None => continue,
        };
        let orig_end_char = norm_end_to_orig(orig_to_norm, orig_start_char, norm_end);
        let start_byte: usize = orig_chars[..orig_start_char].iter().map(|c| c.len_utf8()).sum();
        let end_byte: usize = orig_chars[..orig_end_char].iter().map(|c| c.len_utf8()).sum();
        results.push((start_byte, end_byte));
    }
    results
}

fn map_normalized_positions(
    original: &str,
    normalized: &str,
    normalized_matches: &[Span],
) -> Vec<Span> {
    let mut orig_to_norm = Vec::new();
    let orig_bytes = original.as_bytes();
    let norm_bytes = normalized.as_bytes();
    let mut orig_idx = 0;
    let mut norm_idx = 0;

    while orig_idx < orig_bytes.len() && norm_idx < norm_bytes.len() {
        if orig_bytes[orig_idx] == norm_bytes[norm_idx] {
            orig_to_norm.push(norm_idx);
            orig_idx += 1;
            norm_idx += 1;
        } else if (orig_bytes[orig_idx] == b' ' || orig_bytes[orig_idx] == b'\t')
            && norm_bytes[norm_idx] == b' '
        {
            orig_to_norm.push(norm_idx);
            orig_idx += 1;
            if orig_idx < orig_bytes.len()
                && orig_bytes[orig_idx] != b' '
                && orig_bytes[orig_idx] != b'\t'
            {
                norm_idx += 1;
            }
        } else {
            orig_to_norm.push(norm_idx);
            orig_idx += 1;
        }
    }
    while orig_to_norm.len() < orig_bytes.len() {
        orig_to_norm.push(normalized.len());
    }

    let mut norm_to_orig_start: HashMap<usize, usize> = HashMap::new();
    let mut norm_to_orig_end: HashMap<usize, usize> = HashMap::new();
    for (orig_pos, &norm_pos) in orig_to_norm.iter().enumerate() {
        norm_to_orig_start.entry(norm_pos).or_insert(orig_pos);
        norm_to_orig_end.insert(norm_pos, orig_pos);
    }

    let mut original_matches = Vec::new();
    for &(norm_start, norm_end) in normalized_matches {
        let orig_start = if let Some(&s) = norm_to_orig_start.get(&norm_start) {
            s
        } else {
            orig_to_norm
                .iter()
                .position(|&n| n >= norm_start)
                .unwrap_or(orig_bytes.len())
        };

        let mut orig_end = if norm_end > 0 && norm_to_orig_end.contains_key(&(norm_end - 1)) {
            norm_to_orig_end[&(norm_end - 1)] + 1
        } else {
            orig_start + (norm_end - norm_start)
        };

        if norm_end < normalized.len() && norm_bytes.get(norm_end - 1) == Some(&b' ') {
            while orig_end < original.len()
                && (orig_bytes[orig_end] == b' ' || orig_bytes[orig_end] == b'\t')
            {
                orig_end += 1;
            }
        }
        original_matches.push((orig_start, std::cmp::min(orig_end, original.len())));
    }

    original_matches
}

// ── The 9 Exact Strategies ─────────────────────────────────────────────────

/// Strategy 1: exact, non-overlapping occurrences (str.replace semantics).
pub fn strategy_exact(content: &str, pattern: &str) -> Vec<Span> {
    if pattern.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut start = 0;
    while let Some(pos) = content[start..].find(pattern) {
        let abs_pos = start + pos;
        let end_pos = abs_pos + pattern.len();
        spans.push((abs_pos, end_pos));
        start = end_pos; // non-overlapping advance!
    }
    spans
}

/// Strategy 2: strip each line before comparing.
pub fn strategy_line_trimmed(content: &str, pattern: &str) -> Vec<Span> {
    match_transformed_lines(content, pattern, |ls| {
        ls.iter().map(|l| l.trim().to_string()).collect()
    })
}

/// Strategy 3: collapse runs of spaces/tabs to a single space.
pub fn strategy_whitespace_normalized(content: &str, pattern: &str) -> Vec<Span> {
    fn normalize(s: &str) -> String {
        let mut out = String::new();
        let mut in_ws = false;
        for c in s.chars() {
            if c == ' ' || c == '\t' {
                if !in_ws {
                    out.push(' ');
                    in_ws = true;
                }
            } else {
                in_ws = false;
                out.push(c);
            }
        }
        out
    }

    let content_norm = normalize(content);
    let pattern_norm = normalize(pattern);
    let matches_in_norm = strategy_exact(&content_norm, &pattern_norm);
    if matches_in_norm.is_empty() {
        return Vec::new();
    }
    map_normalized_positions(content, &content_norm, &matches_in_norm)
}

/// Strategy 4: ignore leading indentation entirely.
pub fn strategy_indentation_flexible(content: &str, pattern: &str) -> Vec<Span> {
    match_transformed_lines(content, pattern, |ls| {
        ls.iter().map(|l| l.trim_start().to_string()).collect()
    })
}

/// Strategy 5: treat literal `\n`/`\t`/`\r` in pattern as control chars.
pub fn strategy_escape_normalized(content: &str, pattern: &str) -> Vec<Span> {
    let pattern_unescaped = pattern
        .replace("\\n", "\n")
        .replace("\\t", "\t")
        .replace("\\r", "\r");
    if pattern_unescaped == pattern {
        return Vec::new();
    }
    strategy_exact(content, &pattern_unescaped)
}

/// Strategy 6: strip whitespace on the first and last lines only.
pub fn strategy_trimmed_boundary(content: &str, pattern: &str) -> Vec<Span> {
    match_transformed_lines(content, pattern, strip_boundary)
}

/// Strategy 7: exact/line-trimmed match after Unicode->ASCII normalisation of both sides.
pub fn strategy_unicode_normalized(content: &str, pattern: &str) -> Vec<Span> {
    let norm_content = unicode_normalize(content);
    let norm_pattern = unicode_normalize(pattern);
    if norm_content == content && norm_pattern == pattern {
        return Vec::new();
    }
    let mut norm_matches = strategy_exact(&norm_content, &norm_pattern);
    if norm_matches.is_empty() {
        norm_matches = strategy_line_trimmed(&norm_content, &norm_pattern);
    }
    if norm_matches.is_empty() {
        return Vec::new();
    }

    let orig_to_norm = build_orig_to_norm_map(content);
    let orig_chars: Vec<char> = content.chars().collect();
    map_positions_norm_to_orig(&orig_to_norm, &norm_matches, &orig_chars)
}

/// Strategy 8: anchor on first+last lines, similarity-score the middle.
pub fn strategy_block_anchor(content: &str, pattern: &str) -> Vec<Span> {
    let norm_pattern = unicode_normalize(pattern);
    let pattern_lines: Vec<&str> = norm_pattern.split('\n').collect();
    if pattern_lines.len() < 2 {
        return Vec::new();
    }
    let first_line = pattern_lines[0].trim();
    let last_line = pattern_lines[pattern_lines.len() - 1].trim();
    let n = pattern_lines.len();

    let norm_content = unicode_normalize(content);
    let norm_content_lines: Vec<&str> = norm_content.split('\n').collect();

    let mut potential_matches = Vec::new();
    if norm_content_lines.len() >= n {
        for i in 0..=(norm_content_lines.len() - n) {
            if norm_content_lines[i].trim() == first_line
                && norm_content_lines[i + n - 1].trim() == last_line
            {
                potential_matches.push(i);
            }
        }
    }

    let threshold = if potential_matches.len() == 1 { 0.50 } else { 0.70 };
    let pattern_middle = pattern_lines[1..n - 1].join("\n");

    let content_lines: Vec<&str> = content.split('\n').collect();
    window_spans(content, &content_lines, n, |i| {
        if !potential_matches.contains(&i) {
            return false;
        }
        if n <= 2 {
            return true;
        }
        let content_middle = norm_content_lines[i + 1..i + n - 1].join("\n");
        let sm = SequenceMatcher::new(&content_middle, &pattern_middle);
        sm.ratio() >= threshold
    })
}

/// Strategy 9 (last resort): anchored per-line similarity, every non-blank line >= 0.80.
pub fn strategy_context_aware(content: &str, pattern: &str) -> Vec<Span> {
    let pattern_lines: Vec<&str> = pattern.split('\n').collect();
    let content_lines: Vec<&str> = content.split('\n').collect();
    let n = pattern_lines.len();
    if n > content_lines.len() || n == 0 {
        return Vec::new();
    }
    let first_pat = pattern_lines[0].trim();
    let last_pat = pattern_lines[n - 1].trim();

    fn sim(a: &str, b: &str) -> f64 {
        if a == b {
            1.0
        } else {
            SequenceMatcher::new(a, b).ratio()
        }
    }

    window_spans(content, &content_lines, n, |i| {
        let block_lines = &content_lines[i..i + n];
        if sim(first_pat, block_lines[0].trim()) < 0.80 {
            return false;
        }
        if sim(last_pat, block_lines[n - 1].trim()) < 0.80 {
            return false;
        }
        for (p_line, c_line) in pattern_lines.iter().zip(block_lines.iter()) {
            let p_trim = p_line.trim();
            if !p_trim.is_empty() && sim(p_trim, c_line.trim()) < 0.80 {
                return false;
            }
        }
        true
    })
}

// ── Orchestrator Helpers ───────────────────────────────────────────────────

pub fn is_already_applied(content: &str, old_string: &str, new_string: &str) -> bool {
    if new_string.is_empty() || new_string.trim().len() < 8 || !content.contains(new_string) {
        return false;
    }
    old_string == new_string || !content.contains(old_string)
}

fn matched_regions(content: &str, matches: &[Span]) -> String {
    let mut out = String::new();
    for &(start, end) in matches {
        if start <= end && end <= content.len() {
            out.push_str(&content[start..end]);
        }
    }
    out
}

pub fn format_match_locations(content: &str, matches: &[Span], cap: usize) -> String {
    let mut rows = Vec::new();
    for &(start, _) in matches.iter().take(cap) {
        let line_no = content[..start].chars().filter(|&c| c == '\n').count() + 1;
        let line_start = content[..start].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let line_end = content[line_start..]
            .find('\n')
            .map(|p| line_start + p)
            .unwrap_or(content.len());
        let snippet = content[line_start..line_end].trim();
        let formatted = if snippet.chars().count() > 80 {
            let s: String = snippet.chars().take(77).collect();
            format!("{}...", s)
        } else {
            snippet.to_string()
        };
        rows.push(format!("  L{}: {}", line_no, formatted));
    }
    let extra = matches.len().saturating_sub(cap);
    if extra > 0 {
        rows.push(format!("  ... and {} more", extra));
    }
    rows.join("\n")
}

fn detect_escape_drift(
    content: &str,
    matches: &[Span],
    old_string: &str,
    new_string: &str,
) -> Option<String> {
    let has_quote_suspects = new_string.contains("\\'") || new_string.contains("\\\"");
    if !has_quote_suspects && !old_string.contains('\\') {
        return None;
    }

    let regions = matched_regions(content, matches);
    if has_quote_suspects {
        for suspect in &["\\'", "\\\""] {
            if new_string.contains(suspect)
                && old_string.contains(suspect)
                && !regions.contains(suspect)
            {
                let plain = &suspect[1..];
                return Some(format!(
                    "Escape-drift detected: old_string and new_string contain \
                    the literal sequence {:?} but the matched region of \
                    the file does not. This is almost always a tool-call \
                    serialization artifact where an apostrophe or quote got \
                    prefixed with a spurious backslash. Re-read the file with \
                    read_file and pass old_string/new_string without \
                    backslash-escaping {:?} characters.",
                    suspect, plain
                ));
            }
        }
    }
    detect_backslash_doubling(&regions, old_string, new_string)
}

fn backslash_runs(s: &str) -> Vec<usize> {
    let mut runs = Vec::new();
    let mut count = 0;
    for c in s.chars() {
        if c == '\\' {
            count += 1;
        } else if count > 0 {
            runs.push(count);
            count = 0;
        }
    }
    if count > 0 {
        runs.push(count);
    }
    runs
}

fn detect_backslash_doubling(
    matched_reg: &str,
    old_string: &str,
    new_string: &str,
) -> Option<String> {
    let old_runs = backslash_runs(old_string);
    let file_runs = backslash_runs(matched_reg);
    if old_runs.is_empty()
        || file_runs.is_empty()
        || old_runs.len() != file_runs.len()
        || old_runs == file_runs
        || old_runs.iter().zip(&file_runs).any(|(&o, &f)| o != f * 2)
        || !(file_runs.iter().any(|&f| f >= 2) || file_runs.len() >= 2)
        || backslash_runs(new_string) == file_runs
    {
        return None;
    }
    Some(
        "Escape-drift detected: every backslash run in old_string is exactly \
        twice as long as in the matched region of the file (e.g. the file \
        has `\\\\` where old_string has `\\\\\\\\`). The tool-call arguments \
        were JSON-escaped one extra time; applying new_string verbatim would \
        double every backslash in the file. Re-read the file with read_file \
        and resend old_string/new_string with the backslash counts exactly \
        as they appear in the file."
            .to_string(),
    )
}

fn maybe_unescape_new_string(new_string: &str, content: &str, matches: &[Span]) -> String {
    if !new_string.contains("\\t") && !new_string.contains("\\r") {
        return new_string.to_string();
    }
    let regions = matched_regions(content, matches);
    let mut result = new_string.to_string();
    for (literal, control) in &[("\\t", "\t"), ("\\r", "\r")] {
        if result.contains(literal) && regions.contains(control) {
            result = result.replace(literal, control);
        }
    }
    result
}

fn leading_whitespace(line: &str) -> &str {
    let trimmed = line.trim_start_matches(|c| c == ' ' || c == '\t');
    &line[..line.len() - trimmed.len()]
}

fn first_meaningful_line(text: &str) -> Option<&str> {
    text.split('\n').find(|l| !l.trim().is_empty())
}

fn reindent_replacement(file_region: &str, old_string: &str, new_string: &str) -> String {
    if new_string.is_empty() {
        return new_string.to_string();
    }
    let old_first = match first_meaningful_line(old_string) {
        Some(f) => f,
        None => return new_string.to_string(),
    };
    let file_first = match first_meaningful_line(file_region) {
        Some(f) => f,
        None => return new_string.to_string(),
    };

    let old_indent = leading_whitespace(old_first);
    let file_indent = leading_whitespace(file_first);
    if old_indent == file_indent {
        return new_string.to_string();
    }

    let mut out_lines = Vec::new();
    for line in new_string.split('\n') {
        if line.trim().is_empty() {
            out_lines.push(line.to_string());
        } else if leading_whitespace(line).starts_with(old_indent) {
            out_lines.push(format!("{}{}", file_indent, &line[old_indent.len()..]));
        } else {
            out_lines.push(format!("{}{}", file_indent, line.trim_start_matches(|c| c == ' ' || c == '\t')));
        }
    }
    out_lines.join("\n")
}

fn preserve_unicode_in_replacement(
    content: &str,
    matches: &[Span],
    old_string: &str,
    new_string: &str,
) -> String {
    let file_region = matched_regions(content, matches);
    let norm_old = unicode_normalize(old_string);
    if norm_old != unicode_normalize(&file_region) {
        return new_string.to_string();
    }

    let file_orig_to_norm = build_orig_to_norm_map(&file_region);
    let file_norm_to_orig = invert_norm_map(&file_orig_to_norm);
    let file_region_chars: Vec<char> = file_region.chars().collect();

    let sm = SequenceMatcher::new(&norm_old, new_string);
    let mut result_parts = Vec::new();

    for op in sm.get_opcodes() {
        match op.tag {
            OpcodeTag::Equal => {
                let orig_start_char = file_norm_to_orig.get(&op.i1).copied().unwrap_or(0);
                let orig_end_char = norm_end_to_orig(&file_orig_to_norm, orig_start_char, op.i2);
                let s: String = file_region_chars[orig_start_char..orig_end_char].iter().collect();
                result_parts.push(s);
            }
            OpcodeTag::Delete => {}
            OpcodeTag::Replace | OpcodeTag::Insert => {
                let new_chars: Vec<char> = new_string.chars().collect();
                let s: String = new_chars[op.j1..op.j2].iter().collect();
                result_parts.push(s);
            }
        }
    }

    result_parts.join("")
}

fn apply_replacements(
    content: &str,
    matches: &[Span],
    new_string: &str,
    old_string: Option<&str>,
) -> String {
    let mut sorted_matches = matches.to_vec();
    sorted_matches.sort_by_key(|m| std::cmp::Reverse(m.0));

    let mut result = content.to_string();
    for (start, end) in sorted_matches {
        let mut adjusted = new_string.to_string();
        if let Some(old) = old_string {
            adjusted = reindent_replacement(&content[start..end], old, new_string);
        }
        result = format!("{}{}{}", &result[..start], adjusted, &result[end..]);
    }
    result
}

// ── "Did you mean?" diagnostics ──────────────────────────────────────────

fn visualize_whitespace(line: &str) -> String {
    let stripped = line.trim_start_matches(|c| c == ' ' || c == '\t');
    let prefix = &line[..line.len() - stripped.len()];
    let vis_prefix = prefix.replace('\t', "→").replace(' ', "·");
    format!("{}{}", vis_prefix, stripped)
}

pub fn find_closest_lines(
    old_string: &str,
    content: &str,
    context_lines: usize,
    max_results: usize,
) -> String {
    if old_string.is_empty() || content.is_empty() {
        return String::new();
    }
    let old_lines: Vec<&str> = old_string.lines().collect();
    let content_lines: Vec<&str> = content.lines().collect();
    if old_lines.is_empty() || content_lines.is_empty() {
        return String::new();
    }

    let anchor = if !old_lines[0].trim().is_empty() {
        old_lines[0].trim()
    } else {
        match old_lines.iter().find(|l| !l.trim().is_empty()) {
            Some(l) => l.trim(),
            None => return String::new(),
        }
    };

    let mut scored: Vec<(f64, usize)> = content_lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(i, line)| {
            let r = SequenceMatcher::new(anchor, line.trim()).ratio();
            (r, i)
        })
        .collect();

    scored.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal));
    let top: Vec<(f64, usize)> = scored
        .into_iter()
        .filter(|(r, _)| *r > 0.3)
        .take(max_results)
        .collect();

    if top.is_empty() {
        return String::new();
    }

    let mut parts = Vec::new();
    let mut seen_ranges: Vec<(usize, usize)> = Vec::new();

    for &(_, line_idx) in &top {
        let start = line_idx.saturating_sub(context_lines);
        let end = std::cmp::min(content_lines.len(), line_idx + old_lines.len() + context_lines);
        if seen_ranges.iter().any(|&(s, e)| s == start && e == end) {
            continue;
        }
        seen_ranges.push((start, end));

        let mut block = Vec::new();
        for j in 0..(end - start) {
            let line_num = start + j + 1;
            block.push(format!("{:4}| {}", line_num, content_lines[start + j]));
        }
        parts.push(block.join("\n"));
    }

    let mut result = parts.join("\n---\n");

    let best_line = content_lines[top[0].1];
    if best_line.trim() == anchor && best_line != old_lines[0] {
        result.push_str("\n\nWhitespace difference detected (→ = tab, · = space):\n");
        result.push_str(&format!("  file has: {}\n", visualize_whitespace(best_line)));
        result.push_str(&format!("  you sent: {}\n", visualize_whitespace(old_lines[0])));
        result.push_str("Use the exact whitespace shown in 'file has'.");
    }

    result
}

pub fn format_no_match_hint(
    error: Option<&str>,
    match_count: usize,
    old_string: &str,
    content: &str,
) -> String {
    if match_count != 0 {
        return String::new();
    }
    match error {
        Some(e) if e.starts_with("Could not find") => {
            let hint = find_closest_lines(old_string, content, 2, 3);
            if !hint.is_empty() {
                format!("\n\nDid you mean one of these sections?\n{}", hint)
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

// ── Main Orchestrator ──────────────────────────────────────────────────────

pub fn fuzzy_find_and_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> (String, usize, Option<String>, Option<String>) {
    if old_string.is_empty() {
        return (
            content.to_string(),
            0,
            None,
            Some(
                "old_string is empty — nothing to match. Set old_string to the exact \
                existing text the replacement should replace (read the file first if \
                unsure). To create a new file or fully rewrite one, use write_file \
                instead. Do not re-send this call unchanged."
                    .to_string(),
            ),
        );
    }

    if old_string.trim().is_empty() {
        return (
            content.to_string(),
            0,
            None,
            Some(
                "old_string is only whitespace — provide non-blank text to match. Set it \
                to the exact existing text the replacement should replace (read the file \
                first if unsure). Do not re-send this call unchanged."
                    .to_string(),
            ),
        );
    }

    if old_string == new_string {
        return (
            content.to_string(),
            0,
            None,
            Some(IDENTICAL_STRINGS_ERROR.to_string()),
        );
    }

    let strategies: &[(&str, fn(&str, &str) -> Vec<Span>)] = &[
        ("exact", strategy_exact),
        ("line_trimmed", strategy_line_trimmed),
        ("whitespace_normalized", strategy_whitespace_normalized),
        ("indentation_flexible", strategy_indentation_flexible),
        ("escape_normalized", strategy_escape_normalized),
        ("trimmed_boundary", strategy_trimmed_boundary),
        ("unicode_normalized", strategy_unicode_normalized),
        ("block_anchor", strategy_block_anchor),
        ("context_aware", strategy_context_aware),
    ];

    for &(strategy_name, strategy_fn) in strategies {
        let matches = strategy_fn(content, old_string);
        if matches.is_empty() {
            continue;
        }

        if matches.len() > 1 && !replace_all {
            let locations = format_match_locations(content, &matches, 5);
            return (
                content.to_string(),
                0,
                None,
                Some(format!(
                    "Found {} matches for old_string. \
                    Provide more context to make it unique, or use replace_all=True. \
                    Matches:\n{}",
                    matches.len(),
                    locations
                )),
            );
        }

        if replace_all && matches.len() > 1 && (strategy_name == "block_anchor" || strategy_name == "context_aware") {
            return (
                content.to_string(),
                0,
                None,
                Some(format!(
                    "Found {} approximate matches via the \
                    '{}' strategy; replace_all only applies to exact \
                    matches. Provide the precise text (whitespace included) so an \
                    exact/line-trimmed match can be made.",
                    matches.len(),
                    strategy_name
                )),
            );
        }

        if strategy_name != "exact" {
            if let Some(drift_err) = detect_escape_drift(content, &matches, old_string, new_string) {
                return (content.to_string(), 0, None, Some(drift_err));
            }
        }

        let mut effective_new = maybe_unescape_new_string(new_string, content, &matches);
        if strategy_name == "unicode_normalized" {
            effective_new = preserve_unicode_in_replacement(
                content,
                &matches,
                old_string,
                &effective_new,
            );
        }

        let new_content = apply_replacements(
            content,
            &matches,
            &effective_new,
            if strategy_name != "exact" { Some(old_string) } else { None },
        );

        return (
            new_content,
            matches.len(),
            Some(strategy_name.to_string()),
            None,
        );
    }

    (
        content.to_string(),
        0,
        None,
        Some("Could not find a match for old_string in the file".to_string()),
    )
}

/// Unified diff generation matching Python difflib unified_diff
pub fn unified_diff(old_content: &str, new_content: &str, filename: &str) -> String {
    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();

    let mut diff = Vec::new();
    diff.push(format!("--- a/{}", filename));
    diff.push(format!("+++ b/{}", filename));

    let mut i = 0;
    let mut j = 0;
    let mut hunks: Vec<String> = Vec::new();

    while i < old_lines.len() || j < new_lines.len() {
        if i < old_lines.len() && j < new_lines.len() && old_lines[i] == new_lines[j] {
            i += 1;
            j += 1;
        } else {
            let start_i = i;
            let start_j = j;
            let mut del_lines = Vec::new();
            let mut add_lines = Vec::new();

            // Find next matching line
            let mut found_sync = false;
            let lookahead = 20;
            for di in 0..lookahead {
                for dj in 0..lookahead {
                    if di == 0 && dj == 0 {
                        continue;
                    }
                    if start_i + di < old_lines.len()
                        && start_j + dj < new_lines.len()
                        && old_lines[start_i + di] == new_lines[start_j + dj]
                    {
                        for idx in start_i..start_i + di {
                            del_lines.push(format!("-{}", old_lines[idx]));
                        }
                        for idx in start_j..start_j + dj {
                            add_lines.push(format!("+{}", new_lines[idx]));
                        }
                        i = start_i + di;
                        j = start_j + dj;
                        found_sync = true;
                        break;
                    }
                }
                if found_sync {
                    break;
                }
            }

            if !found_sync {
                // Consume rest
                while i < old_lines.len() {
                    del_lines.push(format!("-{}", old_lines[i]));
                    i += 1;
                }
                while j < new_lines.len() {
                    add_lines.push(format!("+{}", new_lines[j]));
                    j += 1;
                }
            }

            let ctx_before = start_i.saturating_sub(3);
            let ctx_after = std::cmp::min(old_lines.len(), i + 3);

            let old_count = (ctx_after - ctx_before) + del_lines.len();
            let new_count = (ctx_after - ctx_before) + add_lines.len();

            let mut hunk = Vec::new();
            hunk.push(format!(
                "@@ -{},{} +{},{} @@",
                ctx_before + 1,
                old_count,
                ctx_before + 1,
                new_count
            ));
            for idx in ctx_before..start_i {
                hunk.push(format!(" {}", old_lines[idx]));
            }
            hunk.extend(del_lines);
            hunk.extend(add_lines);
            for idx in i..ctx_after {
                hunk.push(format!(" {}", old_lines[idx]));
            }
            hunks.push(hunk.join("\n"));
        }
    }

    if hunks.is_empty() {
        return String::new();
    }

    diff.extend(hunks);
    diff.join("\n") + "\n"
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strategy_1_exact() {
        let content = "hello world\nline 2\nhello world";
        let (_new, count, _strategy, err) =
            fuzzy_find_and_replace(content, "hello world", "hi universe", false);
        assert_eq!(count, 0);
        assert!(err.unwrap().contains("Found 2 matches"));

        let (new_all, count_all, strategy_all, err_all) =
            fuzzy_find_and_replace(content, "hello world", "hi universe", true);
        assert!(err_all.is_none());
        assert_eq!(count_all, 2);
        assert_eq!(strategy_all.as_deref(), Some("exact"));
        assert_eq!(new_all, "hi universe\nline 2\nhi universe");
    }

    #[test]
    fn test_strategy_2_line_trimmed() {
        let content = "   def calculate():   \n       return 42   ";
        let old = "def calculate():\n    return 42";
        let new_s = "def calculate():\n    return 99";
        let (new, count, strategy, err) = fuzzy_find_and_replace(content, old, new_s, false);
        assert!(err.is_none(), "err: {:?}", err);
        assert_eq!(count, 1);
        assert_eq!(strategy.as_deref(), Some("line_trimmed"));
        assert!(new.contains("return 99"));
    }

    #[test]
    fn test_strategy_3_whitespace_normalized() {
        let content = "foo   bar\tbaz";
        let old = "foo bar baz";
        let new_s = "foo bar qux";
        let (new, count, strategy, err) = fuzzy_find_and_replace(content, old, new_s, false);
        assert!(err.is_none(), "err: {:?}", err);
        assert_eq!(count, 1);
        assert_eq!(strategy.as_deref(), Some("whitespace_normalized"));
        assert_eq!(new, "foo bar qux");
    }

    #[test]
    fn test_strategy_4_indentation_flexible_unit() {
        let content = "    def foo():\n        return 1";
        let pattern = "def foo():\n    return 1";
        let spans = strategy_indentation_flexible(content, pattern);
        assert_eq!(spans.len(), 1);
        assert_eq!(&content[spans[0].0..spans[0].1], content);
    }

    #[test]
    fn test_strategy_5_escape_normalized() {
        let content = "line 1\nline 2\nline 3";
        let old = "line 1\\nline 2";
        let new_s = "line 1\\nline 2 modified";
        let (new, count, strategy, err) = fuzzy_find_and_replace(content, old, new_s, false);
        assert!(err.is_none(), "err: {:?}", err);
        assert_eq!(count, 1);
        assert_eq!(strategy.as_deref(), Some("escape_normalized"));
        assert!(new.contains("line 2 modified"));
    }

    #[test]
    fn test_strategy_6_trimmed_boundary_unit() {
        let content = "  first line  \n    middle line\n  last line  ";
        let pattern = "first line\n    middle line\nlast line";
        let spans = strategy_trimmed_boundary(content, pattern);
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn test_strategy_7_unicode_normalized() {
        let content = "offset = value \u{2212} 1\nprint(offset)\n";
        let old = "offset = value - 1";
        let new_s = "offset = delta - 1";
        let (new, count, strategy, err) = fuzzy_find_and_replace(content, old, new_s, false);
        assert!(err.is_none(), "err: {:?}", err);
        assert_eq!(count, 1);
        assert_eq!(strategy.as_deref(), Some("unicode_normalized"));
        assert!(new.contains("delta \u{2212} 1"));
    }

    #[test]
    fn test_strategy_8_block_anchor() {
        let content = "START\nline A\nline B\nEND";
        let old = "START\nline A changed\nline B\nEND";
        let new_s = "START\nall changed\nEND";
        let (new, count, strategy, err) = fuzzy_find_and_replace(content, old, new_s, false);
        assert!(err.is_none(), "err: {:?}", err);
        assert_eq!(count, 1);
        assert_eq!(strategy.as_deref(), Some("block_anchor"));
        assert_eq!(new, "START\nall changed\nEND");
    }

    #[test]
    fn test_strategy_9_context_aware() {
        let content = "console.log(\"hello world!\");";
        let old = "console.log(\"hello world;\");";
        let new_s = "console.log(\"hi\");";
        let (new, count, strategy, err) = fuzzy_find_and_replace(content, old, new_s, false);
        assert!(err.is_none(), "err: {:?}", err);
        assert_eq!(count, 1);
        assert_eq!(strategy.as_deref(), Some("context_aware"));
        assert_eq!(new, "console.log(\"hi\");");
    }

    #[test]
    fn test_is_already_applied() {
        let content = "def test():\n    let x = 42;\n    return x;";
        assert!(is_already_applied(content, "let x = 10;", "let x = 42;"));
        assert!(!is_already_applied(content, "let x = 10;", "let x = 99;"));
    }

    #[test]
    fn test_escape_drift_apostrophe() {
        let content = "line\n    x = 1\nline";
        let old_string = "line\n  x = \\'a\\'\nline";
        let new_string = "line\n  x = \\'b\\'\nline";
        let (new, count, strategy, err) =
            fuzzy_find_and_replace(content, old_string, new_string, false);
        assert_eq!(count, 0);
        assert!(strategy.is_none());
        assert!(err.is_some());
        assert!(err.unwrap().contains("Escape-drift"));
        assert_eq!(new, content);
    }
}
