//! Geometry for the view: the minimum terminal, the centred content column,
//! and word wrapping that the view measures before it draws (so a box is as
//! tall as its text, never stretched to the terminal).

use super::Key;
use ratatui::layout::{Rect, Size};

/// Below this the wizard shows only "terminal too small".
pub const MIN_W: u16 = 80;
pub const MIN_H: u16 = 24;
/// The content column never gets wider than this, however wide the terminal.
pub const MAX_W: u16 = 100;

pub fn fits(s: Size) -> bool {
    s.width >= MIN_W && s.height >= MIN_H
}

/// Keys act only on what is on screen: while the terminal is too small to
/// show the wizard, only Ctrl-C (quit, or stop a running command) gets through.
pub fn key_allowed(s: Size, k: Key) -> bool {
    fits(s) || k == Key::CtrlC
}

/// A rectangle of at most `w` x `h`, centred in `area`.
pub fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}

/// The full-height content column, at most [`MAX_W`] wide, centred.
pub fn column(area: Rect) -> Rect {
    let w = area.width.min(MAX_W);
    Rect::new(area.x + (area.width - w) / 2, area.y, w, area.height)
}

pub fn width(s: &str) -> usize {
    s.chars().count()
}

/// Greedy word wrap to `cols` columns. Explicit newlines stay, runs of
/// spaces inside a line stay (the staged-checks text aligns with them), and
/// continuation lines hang under the text: after a line's indent, after a
/// `- ` bullet, or under the value of a `label:   value` line.
pub fn wrap(text: &str, cols: usize) -> Vec<String> {
    let cols = cols.max(1);
    let mut out = Vec::new();
    for line in text.split('\n') {
        let line = line.trim_end();
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        let lead = line.chars().take_while(|c| *c == ' ').count();
        let indent = " ".repeat(if lead < cols / 2 { lead } else { 0 });
        let hang = " ".repeat(match hang(&line[lead..]) {
            h if lead + h < cols * 2 / 3 => lead + h,
            _ => indent.len(),
        });
        // (spaces before, word) pairs; the first pair's spaces are the indent.
        let mut pairs: Vec<(usize, String)> = Vec::new();
        let mut spaces = 0;
        let mut word = String::new();
        for c in line.chars().skip(lead) {
            if c == ' ' {
                if !word.is_empty() {
                    pairs.push((spaces, std::mem::take(&mut word)));
                    spaces = 0;
                }
                spaces += 1;
            } else {
                word.push(c);
            }
        }
        if !word.is_empty() {
            pairs.push((spaces, word));
        }
        let mut cur = indent.clone();
        let mut fresh = true;
        for (sp, w) in pairs {
            let need = if fresh { 0 } else { sp } + width(&w);
            if !fresh && width(&cur) + need > cols {
                out.push(std::mem::replace(&mut cur, hang.clone()));
                fresh = true;
            }
            if !fresh {
                cur.push_str(&" ".repeat(sp));
            }
            // A word longer than the line is cut.
            let mut rest: Vec<char> = w.chars().collect();
            loop {
                let room = cols.saturating_sub(width(&cur)).max(1);
                if rest.len() <= room {
                    cur.extend(rest);
                    break;
                }
                cur.extend(rest.drain(..room));
                out.push(std::mem::replace(&mut cur, hang.clone()));
            }
            fresh = false;
        }
        out.push(cur);
    }
    out
}

/// How far continuation lines of `body` (a line without its indent) hang.
fn hang(body: &str) -> usize {
    if body.starts_with("- ") || body.starts_with("* ") {
        return 2;
    }
    // `label:` then at least two spaces: under the value.
    let Some(colon) = body.find(':') else {
        return 0;
    };
    let label = &body[..colon];
    let gap = body[colon + 1..].chars().take_while(|c| *c == ' ').count();
    if !label.contains(' ') && gap >= 2 {
        width(label) + 1 + gap
    } else {
        0
    }
}

/// `s` cut to `w` columns, with an ellipsis when it was longer.
pub fn truncate(s: &str, w: usize) -> String {
    if width(s) <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut t: String = s.chars().take(w - 1).collect();
    t.push('…');
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_keeps_words_indent_and_inner_spaces() {
        assert_eq!(
            wrap("aaa bbb ccc", 7),
            vec!["aaa bbb".to_string(), "ccc".into()]
        );
        assert_eq!(
            wrap("  run:    x y", 9),
            vec!["  run:".to_string(), "  x y".into()]
        );
        assert_eq!(
            wrap("abcdefghij", 4),
            vec!["abcd".to_string(), "efgh".into(), "ij".into()]
        );
        assert_eq!(wrap("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(wrap("- aaa bbb", 7), vec!["- aaa", "  bbb"]);
        assert_eq!(
            wrap("  run:   aaaa bbbb cccc", 18),
            vec!["  run:   aaaa bbbb", "         cccc"]
        );
        for l in wrap(super::super::plan::WELCOME, 30) {
            assert!(width(&l) <= 30, "{l}");
        }
    }

    #[test]
    fn column_is_bounded_and_centred() {
        let c = column(Rect::new(0, 0, 200, 75));
        assert_eq!((c.x, c.width, c.height), (50, 100, 75));
        assert_eq!(column(Rect::new(0, 0, 80, 24)).width, 80);
    }

    #[test]
    fn keys_pass_only_when_the_wizard_is_visible() {
        let small = Size::new(60, 15);
        assert!(!key_allowed(small, Key::Enter));
        assert!(!key_allowed(small, Key::Char('q')));
        assert!(key_allowed(small, Key::CtrlC));
        assert!(key_allowed(Size::new(80, 24), Key::Enter));
    }
}
