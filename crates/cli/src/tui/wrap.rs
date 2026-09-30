//! Width-aware wrapping for styled `Line`s — the transcript pre-wraps its
//! own lines so `scroll_back` counts *visual* rows, matching what the user
//! sees. `Paragraph::wrap` would fold at draw time and leave our scroll
//! math counting logical lines instead — the two disagree on wide text.
//!
//! Style-preserving: each wrapped fragment keeps its source span's style.
//! Grapheme-native: iteration and measurement run on `unicode-segmentation`
//! grapheme clusters, so emoji/ZWJ sequences and combining marks move as
//! units and measure as clusters instead of summing raw code points.

use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Display width of `s`, summed over grapheme clusters — the same unit the
/// rest of the renderer should measure in.
pub fn display_width(s: &str) -> usize {
    s.graphemes(true).map(UnicodeWidthStr::width).sum()
}

/// Break `line` into rows no wider than `width` (display columns). Word
/// boundaries are preferred; an unbreakable long word is hard-split at
/// grapheme edges. Empty lines yield one empty row. `width == 0` returns
/// the line unchanged.
pub fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line.clone()];
    }
    // Fast path: nothing to fold.
    if line.width() <= width && !line.spans.iter().any(|s| s.content.contains('\n')) {
        return vec![line.clone()];
    }

    // Flatten spans into (grapheme, style) pairs. Word wrap then operates
    // on display columns; a '\n' inside a span forces a hard break.
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;
    // pending word buffer — flushed when a space or width boundary lands
    let mut word: Vec<Span<'static>> = Vec::new();
    let mut word_w = 0usize;

    fn push_row(
        out: &mut Vec<Line<'static>>,
        cur: &mut Vec<Span<'static>>,
        base: ratatui::style::Style,
    ) {
        let mut l = Line::from(std::mem::take(cur));
        l.style = base;
        out.push(l);
    }

    let flush_word = |word: &mut Vec<Span<'static>>,
                      word_w: &mut usize,
                      cur: &mut Vec<Span<'static>>,
                      cur_w: &mut usize,
                      out: &mut Vec<Line<'static>>,
                      base| {
        if *word_w > width {
            // long word: split hard at width, grapheme by grapheme
            for s in word.drain(..) {
                for g in s.content.as_ref().graphemes(true) {
                    let gw = UnicodeWidthStr::width(g);
                    if *cur_w + gw > width {
                        push_row(out, cur, base);
                        *cur_w = 0;
                    }
                    push_g(cur, g, s.style);
                    *cur_w += gw;
                }
            }
            *word_w = 0;
        } else {
            if *cur_w + *word_w > width && *cur_w > 0 {
                push_row(out, cur, base);
                *cur_w = 0;
            }
            cur.append(word);
            *cur_w += *word_w;
            *word_w = 0;
        }
    };

    let base = line.style;
    for span in &line.spans {
        for g in span.content.as_ref().graphemes(true) {
            if g == "\n" {
                // hard break: flush word then row
                flush_word(&mut word, &mut word_w, &mut cur, &mut cur_w, &mut out, base);
                push_row(&mut out, &mut cur, base);
                cur_w = 0;
                continue;
            }
            let gw = UnicodeWidthStr::width(g);
            if g.trim().is_empty() && g != "\n" {
                flush_word(&mut word, &mut word_w, &mut cur, &mut cur_w, &mut out, base);
                // a space that doesn't fit just disappears at the wrap edge
                if cur_w + gw <= width {
                    push_g(&mut cur, g, span.style);
                    cur_w += gw;
                }
            } else {
                word.push(Span::styled(g.to_string(), span.style));
                word_w += gw;
            }
        }
    }
    flush_word(&mut word, &mut word_w, &mut cur, &mut cur_w, &mut out, base);
    if !cur.is_empty() || out.is_empty() {
        push_row(&mut out, &mut cur, base);
    }
    out
}

/// Append `g` to the last span when styles match, else push a new span —
/// keeps wrapped rows from exploding into one span per grapheme.
fn push_g(cur: &mut Vec<Span<'static>>, g: &str, style: ratatui::style::Style) {
    if let Some(last) = cur.last_mut()
        && last.style == style {
            let mut s = last.content.to_string();
            s.push_str(g);
            last.content = s.into();
            return;
        }
    cur.push(Span::styled(g.to_string(), style));
}

/// Total visual rows `lines` occupy at `width`.
#[allow(dead_code)] // consumed by upcoming transcript window/scrollbar work
pub fn wrapped_height(lines: &[Line<'static>], width: usize) -> usize {
    lines.iter().map(|l| wrap_line(l, width).len()).sum()
}

/// Width of the longest word — used to sanity-check degenerate narrow widths.
#[allow(dead_code)]
pub fn max_word_width(s: &str) -> usize {
    s.split_whitespace().map(display_width).max().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};

    fn plain(s: &str) -> Line<'static> {
        Line::from(Span::raw(s.to_string()))
    }

    #[test]
    fn short_line_passthrough() {
        let out = wrap_line(&plain("hello"), 80);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].spans[0].content.as_ref(), "hello");
    }

    #[test]
    fn wraps_at_word_boundary() {
        let out = wrap_line(&plain("aa bb cc"), 5);
        let texts: Vec<String> = out
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(texts, vec!["aa bb", "cc"]);
    }

    #[test]
    fn long_word_hard_splits() {
        let out = wrap_line(&plain("abcdefgh"), 3);
        let texts: Vec<String> = out
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert_eq!(texts, vec!["abc", "def", "gh"]);
    }

    #[test]
    fn cjk_counts_double_width() {
        // "你好世界" = 8 columns; width 4 → two rows of 你好/世界
        let out = wrap_line(&plain("你好世界"), 4);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn style_preserved_across_wrap() {
        let red = Style::default().fg(Color::Red);
        let line = Line::from(vec![
            Span::styled("aa ".to_string(), red),
            Span::raw("bb cc".to_string()),
        ]);
        let out = wrap_line(&line, 4);
        assert!(out.len() >= 2);
        assert_eq!(out[0].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn embedded_newline_breaks() {
        let out = wrap_line(&plain("a\nb"), 80);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn empty_line_one_row() {
        assert_eq!(wrap_line(&plain(""), 10).len(), 1);
    }

    #[test]
    fn zwj_emoji_never_splits_mid_cluster() {
        // Family emoji = one cluster; a char-wise split would tear it.
        let fam = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}"; // 👨‍👩‍👧
        let line = plain(&format!("xx {fam} yy"));
        let out = wrap_line(&line, 5);
        // the cluster must land intact on one row — never sliced in half
        assert!(out
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.as_ref().contains(fam))));
        for l in &out {
            let joined: String = l.spans.iter().map(|s| s.content.to_string()).collect();
            assert!(display_width(&joined) <= 5);
        }
    }

    #[test]
    fn combining_marks_stay_attached() {
        // e + combining acute = "é" as two code points, one cluster.
        let out = wrap_line(&plain("e\u{0301}x"), 1);
        let texts: Vec<String> = out
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.to_string()).collect())
            .collect();
        assert!(texts.iter().any(|t| t.contains("e\u{0301}")));
    }
}
