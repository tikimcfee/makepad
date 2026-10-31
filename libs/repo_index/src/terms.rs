//! Tokens: the exact-case identifier, the `makepad_search::fold` of it whole, and its parts
//! split at snake_case, CamelCase, acronyms and digits ([`crate::Tokenize`]).
//!
//! A word is a run of alphanumerics and `_`; everything else separates words, so `::`, `/`,
//! `.`, `!` and `r#` split paths, macro calls and raw identifiers. Each word gives:
//! - [`TermForm::Folded`]: the word case-folded whole (`memory_type`, `vkbuffer`);
//! - [`TermForm::Exact`]: the word as written, only when it differs from its fold (it has an
//!   upper-case letter), so a lower-case word costs one posting, not two;
//! - [`TermForm::Part`]: its folded parts (`memory`, `type`; `vk`, `buffer`), when they differ
//!   from the whole.
//!
//! Queries use the same split. A query word's whole fold is its primary term, matched against
//! whole words and identifier parts alike; its parts are expansions, which rank the nearest
//! hits of an absent term (`end_pass_sized_turtle` reaches `end_turtle`).

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};


use crate::{Chunk, Field, FieldTerms, QueryTerm, TermForm, Tokenize};

/// The tokenizer for chunks and queries.
#[derive(Clone, Copy, Debug, Default)]
pub struct Terms;

impl Tokenize for Terms {
    fn chunk_terms(&self, chunk: &Chunk) -> FieldTerms {
        let f = &chunk.fields;
        // Count each distinct word per field first: folding and splitting then run once per
        // distinct word, and no term is allocated per occurrence.
        let mut seen: HashMap<(usize, &str), u32, BuildHasherDefault<WordHasher>> = HashMap::default();
        let mut lengths = [0u32; 5];
        for (at, text) in [&f.name, &f.path, &f.sig, &f.doc, &f.body].into_iter().enumerate() {
            for word in words(text) {
                lengths[at] += 1;
                *seen.entry((at, word)).or_insert(0) += 1;
            }
        }
        // Every distinct word's terms go into one scratch buffer as spans; no term is its own
        // allocation.
        let mut buf = String::with_capacity(seen.keys().map(|(_, w)| w.len() * 2).sum());
        let mut spans: Vec<(TermForm, u32, u32)> = Vec::with_capacity(4);
        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(4);
        let mut raw: Vec<Raw> = Vec::with_capacity(seen.len() * 2);
        for ((at, word), n) in seen {
            spans.clear();
            push_word_terms(word, &mut buf, &mut spans, &mut ranges);
            for &(form, start, len) in &spans {
                let mut h = WordHasher::default();
                h.write(&buf.as_bytes()[start as usize..(start + len) as usize]);
                let key = ((at as u64) << 60) | ((form_index(form) as u64) << 58) | (h.finish() >> 6);
                raw.push(Raw { key, at, form, start, len, tf: n });
            }
        }
        // Distinct words can share a term (`Foo` and `foo` fold alike; `id` is a part of many):
        // order by (field, form, hash of the text), cheap to compare, then merge equal texts.
        let text = |r: &Raw| &buf[r.start as usize..(r.start + r.len) as usize];
        raw.sort_unstable_by(|a, b| a.key.cmp(&b.key).then_with(|| text(a).cmp(text(b))));
        let mut merged: Vec<Raw> = Vec::with_capacity(raw.len());
        for r in raw {
            match merged.last_mut() {
                Some(prev) if prev.key == r.key && text(prev) == text(&r) => prev.tf += r.tf,
                _ => merged.push(r),
            }
        }
        let mut out = FieldTerms::with_capacity(merged.iter().map(|r| r.len as usize).sum(), merged.len());
        for r in &merged {
            out.push(Field::ALL[r.at], r.form, text(r), r.tf);
        }
        out.lengths = lengths;
        out
    }

    fn query_terms(&self, query_words: &[String]) -> Vec<QueryTerm> {
        let mut out: Vec<QueryTerm> = Vec::new();
        for (at, raw) in query_words.iter().enumerate() {
            let word = at.min(u16::MAX as usize) as u16;
            for sub in words(raw) {
                for (form, text) in word_terms(sub) {
                    let term = QueryTerm { form, text, word };
                    if !out.contains(&term) {
                        out.push(term);
                    }
                }
            }
        }
        out
    }
}

/// A fast, unseeded hasher for a chunk's words (FxHash's multiply-rotate). Only for maps that
/// live inside one call; nothing persisted depends on it.
#[derive(Default)]
pub struct WordHasher(u64);

impl Hasher for WordHasher {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut b = [0u8; 8];
            b[..chunk.len()].copy_from_slice(chunk);
            self.0 = (self.0.rotate_left(5) ^ u64::from_le_bytes(b)).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
        }
    }
    fn write_usize(&mut self, n: usize) {
        self.0 = (self.0.rotate_left(5) ^ n as u64).wrapping_mul(0x51_7c_c1_b7_27_22_0a_95);
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// Index of a field in [`Field::ALL`], [`FieldTerms::lengths`] and [`crate::Posting::tf`].
pub fn field_index(field: Field) -> usize {
    match field {
        Field::Name => 0,
        Field::Path => 1,
        Field::Sig => 2,
        Field::Doc => 3,
        Field::Body => 4,
    }
}

fn form_index(form: TermForm) -> u8 {
    match form {
        TermForm::Exact => 0,
        TermForm::Folded => 1,
        TermForm::Part => 2,
    }
}

/// The words of a text: runs of alphanumerics and `_` holding at least one alphanumeric.
pub fn words(text: &str) -> Box<dyn Iterator<Item = &str> + '_> {
    let real = |w: &&str| !w.is_empty() && w.bytes().any(|b| b != b'_');
    if text.is_ascii() {
        // Byte splitting: the same words for ASCII text, without decoding chars.
        Box::new(text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(real))
    } else {
        Box::new(text.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(real))
    }
}

/// `makepad_search::fold`, with a fast path for ASCII (the same result).
fn fold(word: &str) -> String {
    if word.is_ascii() {
        word.to_ascii_lowercase()
    } else {
        makepad_search::fold(word)
    }
}

/// The terms one word contributes: exact (when it has upper case), parts, folded whole.
pub fn word_terms(word: &str) -> Vec<(TermForm, String)> {
    // The common case: a lower-case ASCII word is its own fold and its own only part.
    if word.bytes().all(|b| b.is_ascii_lowercase()) {
        return vec![(TermForm::Folded, word.to_string())];
    }
    let folded = fold(word);
    let mut out = Vec::with_capacity(4);
    if word != folded {
        out.push((TermForm::Exact, word.to_string()));
    }
    let parts = split_parts(word);
    if !(parts.len() == 1 && parts[0] == folded) {
        for part in parts {
            if !out.iter().any(|(form, t)| *form == TermForm::Part && *t == part) {
                out.push((TermForm::Part, part));
            }
        }
    }
    out.push((TermForm::Folded, folded));
    out
}

/// One term of a chunk while it is built: its text is a span of the scratch buffer.
struct Raw {
    key: u64,
    at: usize,
    form: TermForm,
    start: u32,
    len: u32,
    tf: u32,
}

/// [`word_terms`], written into `buf` as `(form, start, len)` spans instead of strings.
/// `ranges` is scratch space for an ASCII word's parts.
fn push_word_terms(word: &str, buf: &mut String, spans: &mut Vec<(TermForm, u32, u32)>, ranges: &mut Vec<(usize, usize)>) {
    let mut push = |buf: &mut String, form: TermForm, text: &str| {
        let start = buf.len() as u32;
        buf.push_str(text);
        spans.push((form, start, text.len() as u32));
    };
    if word.bytes().all(|b| b.is_ascii_lowercase()) {
        push(buf, TermForm::Folded, word);
        return;
    }
    if !word.is_ascii() {
        for (form, text) in word_terms(word) {
            push(buf, form, &text);
        }
        return;
    }
    // ASCII: the fold is the lower-case word, and two parts are equal when their spellings
    // are equal ignoring case.
    if word.bytes().any(|b| b.is_ascii_uppercase()) {
        push(buf, TermForm::Exact, word);
    }
    split_ascii_ranges(word, ranges);
    let whole = ranges.len() == 1 && ranges[0] == (0, word.len());
    if !whole {
        for (i, &(a, b)) in ranges.iter().enumerate() {
            let part = &word[a..b];
            if ranges[..i].iter().any(|&(x, y)| word[x..y].eq_ignore_ascii_case(part)) {
                continue;
            }
            let start = buf.len() as u32;
            buf.extend(part.bytes().map(|c| c.to_ascii_lowercase() as char));
            spans.push((TermForm::Part, start, part.len() as u32));
        }
    }
    let start = buf.len() as u32;
    buf.extend(word.bytes().map(|c| c.to_ascii_lowercase() as char));
    spans.push((TermForm::Folded, start, word.len() as u32));
}

/// True when a word reads as an identifier rather than an English word: it has `_`, an
/// upper-case letter after its first character (`KeyCode`, `HAL`, `ReBAR`), or a digit next to
/// letters (`vec3`). A capitalized English word (`Vulkan`) is not one.
pub fn identifier_shaped(word: &str) -> bool {
    word.contains('_')
        || word.chars().skip(1).any(char::is_uppercase)
        || (word.chars().any(char::is_numeric) && word.chars().any(char::is_alphabetic))
}

/// The standard English function words (NLTK's English stop-word list), lower case. A query
/// drops them unless it is made of nothing else; a chunk keeps them (they cost no ranking).
pub const FUNCTION_WORDS: &[&str] = &[
    "i", "me", "my", "myself", "we", "our", "ours", "ourselves", "you", "your", "yours", "yourself",
    "yourselves", "he", "him", "his", "himself", "she", "her", "hers", "herself", "it", "its", "itself",
    "they", "them", "their", "theirs", "themselves", "what", "which", "who", "whom", "this", "that",
    "these", "those", "am", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
    "having", "do", "does", "did", "doing", "a", "an", "the", "and", "but", "if", "or", "because", "as",
    "until", "while", "of", "at", "by", "for", "with", "about", "against", "between", "into", "through",
    "during", "before", "after", "above", "below", "to", "from", "up", "down", "in", "out", "on", "off",
    "over", "under", "again", "further", "then", "once", "here", "there", "when", "where", "why", "how",
    "all", "any", "both", "each", "few", "more", "most", "other", "some", "such", "no", "nor", "not",
    "only", "own", "same", "so", "than", "too", "very", "s", "t", "can", "will", "just", "don", "should",
    "now", "d", "ll", "m", "o", "re", "ve", "y", "ain", "aren", "couldn", "didn", "doesn", "hadn",
    "hasn", "haven", "isn", "ma", "mightn", "mustn", "needn", "shan", "shouldn", "wasn", "weren", "won",
    "wouldn",
];

/// A lower-case English function word ([`FUNCTION_WORDS`]).
pub fn function_word(word: &str) -> bool {
    FUNCTION_WORDS.contains(&word)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    Upper,
    Lower,
    Digit,
    Other,
}

fn class(c: char) -> Class {
    if c.is_uppercase() {
        Class::Upper
    } else if c.is_numeric() {
        Class::Digit
    } else if c.is_alphabetic() {
        Class::Lower
    } else {
        Class::Other
    }
}

/// Folded parts of an identifier: split at `_`, lower→upper (`fooBar`), the end of an acronym
/// before a capitalized word (`HTTPServer` → `http`, `server`), and letter↔digit (`vec3f` →
/// `vec`, `3`, `f`).
pub fn split_parts(word: &str) -> Vec<String> {
    if word.is_ascii() {
        return split_ascii(word);
    }
    let mut parts = Vec::new();
    for piece in word.split('_').filter(|p| !p.is_empty()) {
        let chars: Vec<char> = piece.chars().collect();
        let mut start = 0;
        for i in 1..chars.len() {
            let boundary = match (class(chars[i - 1]), class(chars[i])) {
                (Class::Lower, Class::Upper)
                | (Class::Digit, Class::Upper | Class::Lower)
                | (Class::Upper | Class::Lower, Class::Digit) => true,
                (Class::Upper, Class::Upper) => chars.get(i + 1).is_some_and(|c| class(*c) == Class::Lower),
                _ => false,
            };
            if boundary {
                parts.push(fold(&chars[start..i].iter().collect::<String>()));
                start = i;
            }
        }
        parts.push(fold(&chars[start..].iter().collect::<String>()));
    }
    parts
}

/// [`split_parts`] over bytes, for ASCII words.
fn split_ascii(word: &str) -> Vec<String> {
    let mut ranges = Vec::new();
    split_ascii_ranges(word, &mut ranges);
    ranges.into_iter().map(|(a, b)| word[a..b].to_ascii_lowercase()).collect()
}

/// The byte ranges of an ASCII word's parts (before folding), into `out` (cleared first).
fn split_ascii_ranges(word: &str, out: &mut Vec<(usize, usize)>) {
    out.clear();
    let class = |b: u8| {
        if b.is_ascii_uppercase() {
            Class::Upper
        } else if b.is_ascii_digit() {
            Class::Digit
        } else if b.is_ascii_lowercase() {
            Class::Lower
        } else {
            Class::Other
        }
    };
    let b = word.as_bytes();
    let mut piece = 0;
    while piece < b.len() {
        let end = b[piece..].iter().position(|&c| c == b'_').map_or(b.len(), |e| piece + e);
        if end > piece {
            let mut start = piece;
            for i in piece + 1..end {
                let boundary = match (class(b[i - 1]), class(b[i])) {
                    (Class::Lower, Class::Upper)
                    | (Class::Digit, Class::Upper | Class::Lower)
                    | (Class::Upper | Class::Lower, Class::Digit) => true,
                    (Class::Upper, Class::Upper) => i + 1 < end && class(b[i + 1]) == Class::Lower,
                    _ => false,
                };
                if boundary {
                    out.push((start, i));
                    start = i;
                }
            }
            out.push((start, end));
        }
        piece = end + 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CircleName, Fields, Kind};

    #[test]
    fn splits_snake_camel_acronym_digit() {
        assert_eq!(split_parts("end_pass_sized_turtle"), ["end", "pass", "sized", "turtle"]);
        assert_eq!(split_parts("VkBuffer"), ["vk", "buffer"]);
        assert_eq!(split_parts("HTTPServer"), ["http", "server"]);
        assert_eq!(split_parts("parseHTTP2Frame"), ["parse", "http", "2", "frame"]);
        assert_eq!(split_parts("vec3f"), ["vec", "3", "f"]);
        assert_eq!(split_parts("MAX_TURN"), ["max", "turn"]);
        assert_eq!(split_parts("_private"), ["private"]);
        assert_eq!(split_parts("spin"), ["spin"]);
        assert_eq!(split_parts("Straße_Größe"), ["straße", "größe"]);
    }

    #[test]
    fn words_split_paths_macros_and_raw_idents() {
        let w: Vec<&str> = words("Device::memory_type(bits) gizmo_count! r#type a/b.rs __").collect();
        assert_eq!(w, ["Device", "memory_type", "bits", "gizmo_count", "r", "type", "a", "b", "rs"]);
    }

    #[test]
    fn exact_form_only_with_upper_case() {
        let t = word_terms("KeyCode");
        assert!(t.contains(&(TermForm::Exact, "KeyCode".into())));
        assert!(t.contains(&(TermForm::Folded, "keycode".into())));
        assert!(t.contains(&(TermForm::Part, "key".into())));
        assert!(t.contains(&(TermForm::Part, "code".into())));
        assert_eq!(word_terms("spin"), vec![(TermForm::Folded, "spin".to_string())]);
    }

    /// The terms of each distinct word per field through [`word_terms`] (one `String` per term),
    /// merged and ordered as `chunk_terms` orders them: what the shared buffer must reproduce.
    fn reference_terms(chunk: &Chunk) -> Vec<(Field, TermForm, String, u32)> {
        let f = &chunk.fields;
        let mut seen: HashMap<(usize, &str), u32> = HashMap::new();
        for (at, text) in [&f.name, &f.path, &f.sig, &f.doc, &f.body].into_iter().enumerate() {
            for word in words(text) {
                *seen.entry((at, word)).or_insert(0) += 1;
            }
        }
        let mut raw: Vec<(u64, Field, TermForm, String, u32)> = Vec::new();
        for ((at, word), n) in seen {
            for (form, text) in word_terms(word) {
                let mut h = WordHasher::default();
                h.write(text.as_bytes());
                let key = ((at as u64) << 60) | ((form_index(form) as u64) << 58) | (h.finish() >> 6);
                raw.push((key, Field::ALL[at], form, text, n));
            }
        }
        raw.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.3.cmp(&b.3)));
        let mut out: Vec<(u64, Field, TermForm, String, u32)> = Vec::new();
        for r in raw {
            match out.last_mut() {
                Some(prev) if prev.0 == r.0 && prev.3 == r.3 => prev.4 += r.4,
                _ => out.push(r),
            }
        }
        out.into_iter().map(|(_, field, form, text, tf)| (field, form, text, tf)).collect()
    }

    #[test]
    fn chunk_terms_share_one_buffer_and_match_per_term_strings() {
        let words = "Gizmo gizmo GIZMO HTTPServer vec3f x86_64 __init__ foo_bar_foo FooFoo fooFOO ReBAR a_B_c \
                     id ID Id _ __ 3d Vec3D ÄpfelBaum ÉCOLE_x straße STRASSE κόσμος_ΚΌΣΜΟΣ r#type ids! A::b";
        let chunk = Chunk {
            kind: Kind::File,
            circle: CircleName("makepad".into()),
            path: "a/b.rs".into(),
            label_len: 0,
            span: Default::default(),
            lines: Default::default(),
            name: "HTTPServer".into(),
            gist: String::new(),
            parent: None,
            fields: Fields { name: "HTTPServer".into(), path: "a/HTTP_server/b.rs".into(), sig: words.into(), doc: words.into(), body: format!("{words} {words}") },
            keys: Default::default(),
            flags: Default::default(),
            detail: crate::Detail::None,
        };
        let ft = Terms.chunk_terms(&chunk);
        let got: Vec<(Field, TermForm, String, u32)> = ft.iter().map(|t| (t.field, t.form, t.text.to_string(), t.tf)).collect();
        assert_eq!(got, reference_terms(&chunk));
        assert_eq!(ft.len(), got.len());
        // A copy reads the same terms (spans index the copy's own buffer).
        assert_eq!(ft.clone(), ft);
    }

    #[test]
    fn chunk_terms_count_tf_and_lengths_per_field() {
        let chunk = Chunk {
            kind: Kind::Method,
            circle: CircleName("makepad".into()),
            path: "alpha/src/lib.rs".into(),
            label_len: 0,
            span: Default::default(),
            lines: Default::default(),
            name: "Gizmo::spin".into(),
            gist: String::new(),
            parent: None,
            fields: Fields {
                name: "Gizmo::spin".into(),
                path: "alpha/src/lib.rs".into(),
                sig: "pub fn spin(&mut self, step: u32)".into(),
                doc: "Spins the gizmo by `step`".into(),
                body: "self.angle = self.angle.wrapping_add(clamp_turn(step, MAX_TURN));".into(),
            },
            keys: Default::default(),
            flags: Default::default(),
            detail: Default::default(),
        };
        let ft = Terms.chunk_terms(&chunk);
        assert_eq!(ft.lengths, [2, 4, 7, 5, 8]);
        let tf = |field, form, text: &str| {
            ft.iter().find(|t| t.field == field && t.form == form && t.text == text).map(|t| t.tf)
        };
        assert_eq!(tf(Field::Name, TermForm::Exact, "Gizmo"), Some(1));
        assert_eq!(tf(Field::Name, TermForm::Folded, "gizmo"), Some(1));
        assert_eq!(tf(Field::Body, TermForm::Folded, "angle"), Some(2));
        assert_eq!(tf(Field::Body, TermForm::Part, "clamp"), Some(1));
        assert_eq!(tf(Field::Body, TermForm::Folded, "clamp_turn"), Some(1));
        assert_eq!(tf(Field::Body, TermForm::Exact, "MAX_TURN"), Some(1));
        assert_eq!(tf(Field::Doc, TermForm::Folded, "spins"), Some(1));
    }

    #[test]
    fn query_terms_keep_their_word() {
        let q = Terms.query_terms(&["Device::memory_type".into(), "vulkan".into()]);
        assert!(q.contains(&QueryTerm { form: TermForm::Exact, text: "Device".into(), word: 0 }));
        assert!(q.contains(&QueryTerm { form: TermForm::Folded, text: "memory_type".into(), word: 0 }));
        assert!(q.contains(&QueryTerm { form: TermForm::Part, text: "memory".into(), word: 0 }));
        assert!(q.contains(&QueryTerm { form: TermForm::Folded, text: "vulkan".into(), word: 1 }));
        assert!(!q.iter().any(|t| t.form == TermForm::Exact && t.word == 1));
    }

    #[test]
    fn identifier_shape() {
        assert!(identifier_shaped("memory_type"));
        assert!(identifier_shaped("KeyCode"));
        assert!(identifier_shaped("HAL"));
        assert!(identifier_shaped("vec3"));
        assert!(!identifier_shaped("memory"));
        assert!(!identifier_shaped("Vulkan"));
        assert!(!identifier_shaped("2026"));
        assert!(function_word("what") && function_word("the") && !function_word("vulkan"));
    }
}
