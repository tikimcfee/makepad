//! Commit chunks for a history circle: the message as a commit chunk plus one bullet chunk per
//! message bullet; touched paths in the low-weight path field. Which commits are local-only, and
//! which are visible at a pinned revision, is computed when the circle is opened, never stored.
//! Commits whose touched paths all fall under an exclusion are left out, and excluded paths
//! are dropped from every commit's list.

/// A bullet of a commit message: its first line and its whole text (continuation lines
/// included).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bullet {
    pub head: String,
    pub text: String,
}

/// The message's bullets: lines starting `- ` or `* ` (after up to 3 spaces), each running to
/// the next bullet, a blank line, or a line indented no deeper than its marker.
pub fn bullets(message: &str) -> Vec<Bullet> {
    let mut out: Vec<Bullet> = Vec::new();
    let mut open: Option<(usize, Bullet)> = None;
    for line in message.lines().skip(1) {
        let indent = line.len() - line.trim_start().len();
        let t = line.trim_start();
        let marker = indent <= 3 && (t.starts_with("- ") || t.starts_with("* "));
        if marker {
            out.extend(open.take().map(|(_, b)| b));
            let head = t[2..].trim().to_string();
            open = Some((indent, Bullet { head: head.clone(), text: head }));
        } else if t.is_empty() {
            out.extend(open.take().map(|(_, b)| b));
        } else if let Some((ind, b)) = open.as_mut() {
            if indent > *ind {
                b.text.push('\n');
                b.text.push_str(t);
            } else {
                out.extend(open.take().map(|(_, b)| b));
            }
        }
    }
    out.extend(open.map(|(_, b)| b));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bullets_and_continuations() {
        let m = "work: devhub, JIT fixes\n\n- devhub: runs survive restarts,\n  rolling landings\n* Synth rewind\nnot a bullet\n  - nested is a bullet too\n\n-not-a-bullet\n";
        let b = bullets(m);
        assert_eq!(b.iter().map(|b| b.head.as_str()).collect::<Vec<_>>(), vec!["devhub: runs survive restarts,", "Synth rewind", "nested is a bullet too"]);
        assert_eq!(b[0].text, "devhub: runs survive restarts,\nrolling landings");
        assert!(bullets("subject only").is_empty());
    }
}
