use helix_core::{Tendril, Transaction};
use helix_view::{document::Mode, Editor};
use once_cell::sync::Lazy;

use crate::commands::Context;

const ABBREVIATION_DATA: &str = include_str!("lean_abbreviations_data.scm");

static ABBREVIATIONS: Lazy<Vec<Abbreviation>> = Lazy::new(|| {
    ABBREVIATION_DATA
        .lines()
        .filter_map(parse_abbreviation_line)
        .collect()
});

#[derive(Clone, Debug, PartialEq, Eq)]
struct Abbreviation {
    input: String,
    replacement: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Expansion {
    start: usize,
    end: usize,
    replacement: String,
}

fn is_lean_insert_source(editor: &Editor) -> bool {
    if editor.mode != Mode::Insert {
        return false;
    }

    let (_, doc) = current_ref!(editor);
    doc.path().is_some()
        && (doc.language_name() == Some("lean")
            || doc
                .path()
                .and_then(|path| path.extension())
                .is_some_and(|extension| extension == "lean"))
}

fn insert_string(editor: &mut Editor, text: &str) {
    let (view, doc) = current!(editor);
    let transaction = Transaction::insert(
        doc.text(),
        &doc.selection(view.id).clone().cursors(doc.text().slice(..)),
        Tendril::from(text),
    );
    doc.apply(&transaction, view.id);
}

pub fn lean_abbreviation(cx: &mut Context) {
    expand_before_commit(cx.editor);
}

pub(crate) fn handle_space(cx: &mut Context) -> bool {
    if !is_lean_insert_source(cx.editor) {
        return false;
    }

    expand_before_commit(cx.editor);
    insert_string(cx.editor, " ");
    true
}

pub(crate) fn expand_before_commit(editor: &mut Editor) {
    if !is_lean_insert_source(editor) {
        return;
    }

    let (view, doc) = current!(editor);
    let text = doc.text().slice(..);
    let mut changes = doc
        .selection(view.id)
        .ranges()
        .iter()
        .filter_map(|range| {
            let cursor = range.cursor(text);
            find_expansion(text, cursor).map(|expansion| {
                (
                    expansion.start,
                    expansion.end,
                    Some(Tendril::from(expansion.replacement)),
                )
            })
        })
        .collect::<Vec<_>>();

    if changes.is_empty() {
        return;
    }

    changes.sort_by_key(|(start, end, _)| (*start, *end));
    let transaction = Transaction::change(doc.text(), changes.into_iter());
    doc.apply(&transaction, view.id);
}

fn find_expansion(text: helix_core::RopeSlice<'_>, cursor: usize) -> Option<Expansion> {
    let mut token_start = cursor;
    while token_start > 0 {
        let Some(ch) = text.get_char(token_start - 1) else {
            break;
        };
        if ch.is_whitespace() {
            break;
        }
        token_start -= 1;
    }

    let segment: String = text.slice(token_start..cursor).into();
    if segment.is_empty() {
        return None;
    }

    let replacement = expand_visible_segment(&segment);
    (replacement != segment).then_some(Expansion {
        start: token_start,
        end: cursor,
        replacement,
    })
}

fn expand_visible_segment(segment: &str) -> String {
    let mut out = String::new();
    let mut idx = 0;

    while idx < segment.len() {
        let ch = segment[idx..].chars().next().expect("valid char boundary");
        if ch != '\\' {
            out.push(ch);
            idx += ch.len_utf8();
            continue;
        }

        let after = &segment[idx + 1..];
        if let Some((prefix_len_chars, replacement)) = best_abbrev_prefix(after) {
            out.push_str(&replacement);
            idx += 1 + char_to_byte_idx(after, prefix_len_chars);
        } else {
            out.push('\\');
            idx += 1;
        }
    }

    out
}

fn best_abbrev_prefix(input: &str) -> Option<(usize, String)> {
    let mut best = None;

    if input.starts_with('\\') {
        best = Some((1, "\\".to_string()));
    }

    for abbrev in ABBREVIATIONS.iter() {
        if input.starts_with(&abbrev.input) {
            let len = abbrev.input.chars().count();
            match &best {
                Some((best_len, _)) if *best_len >= len => {}
                _ => best = Some((len, abbrev.replacement.clone())),
            }
        }
    }

    best
}

fn parse_abbreviation_line(line: &str) -> Option<Abbreviation> {
    let line = line.trim();
    if !line.starts_with("(list ") {
        return None;
    }

    let (input, offset) = parse_scheme_string(line, line.find('"')?)?;
    let rest = &line[offset..];
    let start = rest.find('"')? + offset;
    let (replacement, _) = parse_scheme_string(line, start)?;

    Some(Abbreviation { input, replacement })
}

fn parse_scheme_string(input: &str, quote_start: usize) -> Option<(String, usize)> {
    let mut chars = input[quote_start..].chars();
    if chars.next()? != '"' {
        return None;
    }

    let mut escaped = false;
    let mut parsed = String::new();
    let mut consumed = quote_start + 1;
    for ch in input[quote_start + 1..].chars() {
        consumed += ch.len_utf8();
        if escaped {
            parsed.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some((parsed, consumed)),
            _ => parsed.push(ch),
        }
    }

    None
}

fn char_to_byte_idx(input: &str, char_idx: usize) -> usize {
    if char_idx == 0 {
        return 0;
    }

    input
        .char_indices()
        .nth(char_idx)
        .map(|(idx, _)| idx)
        .unwrap_or(input.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_abbreviation_pairs_from_scheme_data() {
        assert_eq!(
            parse_abbreviation_line(r#"(list "all" "∀")"#),
            Some(Abbreviation {
                input: "all".to_string(),
                replacement: "∀".to_string(),
            })
        );
        assert_eq!(
            parse_abbreviation_line(r#"(list "\\" "\\")"#),
            Some(Abbreviation {
                input: "\\".to_string(),
                replacement: "\\".to_string(),
            })
        );
    }

    #[test]
    fn best_prefix_prefers_longest_match() {
        assert_eq!(best_abbrev_prefix("and=ksdl"), Some((4, "≙".to_string())));
    }

    #[test]
    fn best_prefix_prefers_longest_ambiguous_match() {
        assert_eq!(expand_visible_segment("\\and=tail"), "≙tail");
    }

    #[test]
    fn visible_segment_keeps_suffix_after_longest_match() {
        assert_eq!(expand_visible_segment("\\and=ksdl"), "≙ksdl");
    }

    #[test]
    fn visible_segment_keeps_short_suffix_after_longest_match() {
        assert_eq!(expand_visible_segment("\\and=ks"), "≙ks");
    }

    #[test]
    fn visible_expansion_replaces_only_prefix_and_keeps_suffix() {
        let text = helix_core::Rope::from("\\and=ksdl");
        assert_eq!(
            find_expansion(text.slice(..), text.len_chars()),
            Some(Expansion {
                start: 0,
                end: 9,
                replacement: "≙ksdl".to_string(),
            })
        );
    }

    #[test]
    fn double_backslash_collapses_to_single_backslash() {
        let text = helix_core::Rope::from("\\\\");
        assert_eq!(
            find_expansion(text.slice(..), text.len_chars()),
            Some(Expansion {
                start: 0,
                end: 2,
                replacement: "\\".to_string(),
            })
        );
    }

    #[test]
    fn chained_expansion_rewrites_multiple_abbreviations_in_one_token() {
        let text = helix_core::Rope::from("\\a\\b");
        assert_eq!(
            find_expansion(text.slice(..), text.len_chars()),
            Some(Expansion {
                start: 0,
                end: 4,
                replacement: "αβ".to_string(),
            })
        );
    }

    #[test]
    fn chained_visible_segment_expands_left_to_right() {
        assert_eq!(expand_visible_segment("\\a\\b"), "αβ");
    }

    #[test]
    fn no_match_returns_none() {
        let text = helix_core::Rope::from("\\zzzzz");
        assert_eq!(find_expansion(text.slice(..), text.len_chars()), None);
    }
}
