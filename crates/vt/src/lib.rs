//! A small VT100/xterm-compatible terminal emulator used by xcord.
//!
//! It is deliberately UI-agnostic: `Terminal::feed` consumes bytes from an SSH
//! channel, and `Terminal::snapshot` produces a renderable view for GPUI. The
//! same emulator can therefore be unit-tested without a window.

mod cell;
mod color;
mod terminal;

pub use cell::{Cell, Row};
pub use color::{default_palette, Attrs, Color, Flags};
pub use terminal::{
    Cursor, CursorStyle, Modes, MouseEncoding, MouseMode, Snapshot, SnapshotRow, Terminal,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn term(cols: usize, rows: usize) -> Terminal {
        Terminal::new(cols, rows, 100)
    }

    fn feed(terminal: &mut Terminal, input: &str) {
        terminal.feed(input.as_bytes());
    }

    #[test]
    fn prints_text_and_handles_newlines() {
        let mut t = term(20, 4);
        feed(&mut t, "hello\r\nworld");
        let snapshot = t.snapshot(0);
        assert_eq!(snapshot.line_text(0), "hello");
        assert_eq!(snapshot.line_text(1), "world");
        assert_eq!(snapshot.cursor, Some((1, 5)));
    }

    #[test]
    fn wraps_at_the_right_edge() {
        let mut t = term(5, 3);
        feed(&mut t, "abcdef");
        let snapshot = t.snapshot(0);
        assert_eq!(snapshot.line_text(0), "abcde");
        assert_eq!(snapshot.line_text(1), "f");
        assert!(snapshot.lines[0].wrapped);
    }

    #[test]
    fn sgr_sets_colors_and_flags() {
        let mut t = term(20, 3);
        feed(&mut t, "\x1b[1;31;44mX\x1b[0mY");
        let snapshot = t.snapshot(0);
        let x = &snapshot.lines[0].cells[0];
        assert!(x.attrs.flags.contains(Flags::BOLD));
        assert_eq!(x.attrs.fg, Color::Indexed(1));
        assert_eq!(x.attrs.bg, Color::Indexed(4));
        let y = &snapshot.lines[0].cells[1];
        assert_eq!(y.attrs, Attrs::default());
    }

    #[test]
    fn truecolor_and_256_color() {
        let mut t = term(20, 2);
        feed(&mut t, "\x1b[38;2;12;34;56mA\x1b[48;5;196mB\x1b[0m");
        let snapshot = t.snapshot(0);
        assert_eq!(snapshot.lines[0].cells[0].attrs.fg, Color::Rgb(12, 34, 56));
        assert_eq!(snapshot.lines[0].cells[1].attrs.bg, Color::Indexed(196));
    }

    #[test]
    fn wide_characters_occupy_two_cells() {
        let mut t = term(10, 2);
        feed(&mut t, "你好");
        let snapshot = t.snapshot(0);
        assert_eq!(snapshot.lines[0].cells[0].ch, '你');
        assert_eq!(snapshot.lines[0].cells[0].width, 2);
        assert_eq!(snapshot.lines[0].cells[1].width, 0);
        assert_eq!(snapshot.cursor, Some((0, 4)));
    }

    #[test]
    fn erases_and_moves_cursor() {
        let mut t = term(10, 3);
        feed(&mut t, "abcdef\x1b[2D\x1b[K");
        let snapshot = t.snapshot(0);
        assert_eq!(snapshot.line_text(0), "abcd");
    }

    #[test]
    fn scrolls_into_history() {
        let mut t = term(10, 3);
        for index in 0..10 {
            feed(&mut t, &format!("line{index}\r\n"));
        }
        assert!(t.scrollback_len() >= 8);
        let snapshot = t.snapshot(0);
        assert_eq!(snapshot.line_text(0), "line8");
        assert_eq!(snapshot.line_text(1), "line9");
        assert_eq!(snapshot.line_text(2), "");
        let deep = t.snapshot(5);
        assert_eq!(deep.line_text(0), "line3");
    }

    #[test]
    fn alt_screen_restores_main_screen() {
        let mut t = term(10, 3);
        feed(&mut t, "main");
        feed(&mut t, "\x1b[?1049h");
        feed(&mut t, "alt");
        let alt = t.snapshot(0);
        assert_eq!(alt.line_text(0), "alt");
        assert!(alt.alt_screen);
        feed(&mut t, "\x1b[?1049l");
        let main = t.snapshot(0);
        assert_eq!(main.line_text(0), "main");
        assert!(!main.alt_screen);
    }

    #[test]
    fn resize_preserves_content_and_cursor() {
        let mut t = term(10, 3);
        feed(&mut t, "one\r\ntwo\r\nthree");
        t.resize(20, 5);
        let snapshot = t.snapshot(0);
        assert_eq!(snapshot.cols, 20);
        assert_eq!(snapshot.rows, 5);
        assert_eq!(snapshot.line_text(0), "one");
        assert_eq!(snapshot.line_text(2), "three");
        assert_eq!(snapshot.cursor, Some((2, 5)));
    }

    #[test]
    fn osc_sets_title_and_clipboard() {
        let mut t = term(10, 3);
        feed(&mut t, "\x1b]0;my server\x07");
        assert_eq!(t.title(), "my server");
        feed(&mut t, "\x1b]52;c;aGVsbG8=\x07");
        assert_eq!(t.take_clipboard().as_deref(), Some("hello"));
    }

    #[test]
    fn text_selection_maps_viewport() {
        let mut t = term(10, 4);
        feed(&mut t, "hello\r\nworld");
        let text = t.text_in_viewport((0, 1), (1, 3), 0);
        assert_eq!(text, "ello\nwor");
    }

    #[test]
    fn device_status_report() {
        let mut t = term(10, 3);
        feed(&mut t, "ab\x1b[6n");
        let responses = t.take_responses();
        assert_eq!(responses, vec![b"\x1b[1;3R".to_vec()]);
    }

    #[test]
    fn scroll_region_limits_scrolling() {
        let mut t = term(10, 5);
        feed(&mut t, "\x1b[2;4r");
        feed(&mut t, "\x1b[4;1H\nX");
        let snapshot = t.snapshot(0);
        assert_eq!(snapshot.line_text(3), "X");
        assert_eq!(snapshot.line_text(4), "");
    }
}
