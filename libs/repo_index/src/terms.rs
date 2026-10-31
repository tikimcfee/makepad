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
//! hits of an absent term (`recursion_depth_exceeding_limit` reaches `recursion_limit`).

use std::collections::HashMap;

use makepad_search::fold;

use crate::{Chunk, Field, FieldTerms, QueryTerm, TermForm, TermOcc, Tokenize};

/// The tokenizer for chunks and queries.
#[derive(Clone, Copy, Debug, Default)]
pub struct Terms;

impl Tokenize for Terms {
    fn chunk_terms(&self, chunk: &Chunk) -> FieldTerms {
        let f = &chunk.fields;
        let mut counts: HashMap<(usize, TermForm, String), u32> = HashMap::new();
        let mut lengths = [0u32; 5];
        for (at, text) in [&f.name, &f.path, &f.sig, &f.doc, &f.body].into_iter().enumerate() {
            for word in words(text) {
                lengths[at] += 1;
                for (form, term) in word_terms(word) {
                    *counts.entry((at, form, term)).or_insert(0) += 1;
                }
            }
        }
        let mut terms: Vec<TermOcc> = counts
            .into_iter()
            .map(|((at, form, text), tf)| TermOcc { field: Field::ALL[at], form, text, tf })
            .collect();
        terms.sort_by(|a, b| {
            (field_index(a.field), form_index(a.form), &a.text).cmp(&(field_index(b.field), form_index(b.form), &b.text))
        });
        FieldTerms { terms, lengths }
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
pub fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| w.chars().any(char::is_alphanumeric))
}

/// The terms one word contributes: exact (when it has upper case), parts, folded whole.
pub fn word_terms(word: &str) -> Vec<(TermForm, String)> {
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

/// True when a query word reads as an identifier rather than an English word: it has `_`, an
/// upper-case letter, or a digit next to letters. Such words take part in the exact definition
/// short-circuit even inside a multi-word query.
pub fn identifier_shaped(word: &str) -> bool {
    word.contains('_')
        || word.chars().any(char::is_uppercase)
        || (word.chars().any(char::is_numeric) && word.chars().any(char::is_alphabetic))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CircleName, Fields, Kind};

    #[test]
    fn splits_snake_camel_acronym_digit() {
        assert_eq!(split_parts("recursion_depth_exceeding_limit"), ["recursion", "depth", "exceeding", "limit"]);
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
            ft.terms.iter().find(|t| t.field == field && t.form == form && t.text == text).map(|t| t.tf)
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
        assert!(identifier_shaped("vec3"));
        assert!(!identifier_shaped("memory"));
        assert!(!identifier_shaped("2026"));
    }
}
