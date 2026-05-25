//! Input buffer types and helpers for the editor wrap pipeline.
//!
//! `LineEntry`, `SelectionRange`, `ChatSnapshot`, `VisualRow`, and
//! the `wrap_editor` / `wrap_input` / `display_col_to_char_index`
//! utilities. Standalone tests live here to keep `renderer.rs` shorter.

#[cfg(test)]
mod tests {
    use crate::ui::renderer::{VisualRow, display_col_to_char_index, wrap_editor, wrap_input};

    /// wrap_editor: empty buffer → one empty row, cursor at (0, 0).
    #[test]
    fn wrap_editor_empty() {
        let (rows, r, c) = wrap_editor("", 0, 80);
        assert_eq!(rows, vec![String::new()]);
        assert_eq!((r, c), (0, 0));
    }

    #[test]
    fn wrap_editor_no_wrap_short() {
        let (rows, r, c) = wrap_editor("hello", 5, 80);
        assert_eq!(rows, vec!["hello".to_string()]);
        assert_eq!((r, c), (0, 5));
    }

    #[test]
    fn wrap_editor_newlines_split() {
        let (rows, r, c) = wrap_editor("a\nb\ncc", 5, 80);
        assert_eq!(
            rows,
            vec!["a".to_string(), "b".to_string(), "cc".to_string()]
        );
        assert_eq!((r, c), (2, 1));
    }

    #[test]
    fn wrap_editor_soft_wrap() {
        let s = "abcdefghij";
        let (rows, r, c) = wrap_editor(s, 10, 4);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], "abcd");
        assert_eq!(rows[1], "efgh");
        assert_eq!(rows[2], "ij");
        assert_eq!((r, c), (2, 2));
    }

    #[test]
    fn display_col_to_char_index_ascii_round_trip() {
        assert_eq!(display_col_to_char_index("hello", 0), 0);
        assert_eq!(display_col_to_char_index("hello", 3), 3);
        assert_eq!(display_col_to_char_index("hello", 5), 5);
        assert_eq!(display_col_to_char_index("hello", 99), 5);
    }

    #[test]
    fn display_col_to_char_index_cjk_compresses() {
        let s = "日本";
        assert_eq!(display_col_to_char_index(s, 0), 0);
        assert_eq!(display_col_to_char_index(s, 1), 0);
        assert_eq!(display_col_to_char_index(s, 2), 1);
        assert_eq!(display_col_to_char_index(s, 3), 1);
        assert_eq!(display_col_to_char_index(s, 4), 2);
        assert_eq!(display_col_to_char_index(s, 99), 2);
    }

    #[test]
    fn display_col_to_char_index_emoji() {
        let s = "a🦀b";
        assert_eq!(display_col_to_char_index(s, 0), 0);
        assert_eq!(display_col_to_char_index(s, 1), 1);
        assert_eq!(display_col_to_char_index(s, 2), 1);
        assert_eq!(display_col_to_char_index(s, 3), 2);
        assert_eq!(display_col_to_char_index(s, 4), 3);
    }

    fn lines(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn wrap_empty_buffer_has_one_row() {
        let (rows, cr, cc) = wrap_input(&lines(&[""]), 0, 0, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].logical_line, 0);
        assert_eq!((rows[0].char_start, rows[0].char_end), (0, 0));
        assert_eq!((cr, cc), (0, 0));
    }

    #[test]
    fn wrap_short_line_no_split() {
        let (rows, cr, cc) = wrap_input(&lines(&["hi"]), 0, 2, 10);
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].char_start, rows[0].char_end), (0, 2));
        assert_eq!((cr, cc), (0, 2));
    }

    #[test]
    fn wrap_splits_long_line_into_multiple_visual_rows() {
        let (rows, cr, cc) = wrap_input(&lines(&["abcdefghi"]), 0, 5, 3);
        assert_eq!(rows.len(), 3);
        assert_eq!((rows[0].char_start, rows[0].char_end), (0, 3));
        assert_eq!((rows[1].char_start, rows[1].char_end), (3, 6));
        assert_eq!((rows[2].char_start, rows[2].char_end), (6, 9));
        assert_eq!((cr, cc), (1, 2));
    }

    #[test]
    fn wrap_cursor_at_exact_boundary_stays_on_filled_row() {
        let (rows, cr, cc) = wrap_input(&lines(&["abc"]), 0, 3, 3);
        assert_eq!(rows.len(), 1);
        assert_eq!((cr, cc), (0, 3));
    }

    #[test]
    fn wrap_cursor_after_full_row_with_continuation() {
        let (rows, cr, cc) = wrap_input(&lines(&["abcdef"]), 0, 6, 3);
        assert_eq!(rows.len(), 2);
        assert_eq!((cr, cc), (1, 3));
    }

    #[test]
    fn wrap_cursor_at_start_of_continuation_row() {
        let (rows, cr, cc) = wrap_input(&lines(&["abcdef"]), 0, 3, 3);
        assert_eq!(rows.len(), 2);
        assert_eq!((cr, cc), (1, 0));
    }

    #[test]
    fn wrap_multiple_logical_lines() {
        let (rows, cr, cc) = wrap_input(&lines(&["abc", "defgh"]), 1, 4, 3);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].logical_line, 0);
        assert_eq!(rows[1].logical_line, 1);
        assert_eq!(rows[2].logical_line, 1);
        assert_eq!((cr, cc), (2, 1));
    }

    #[test]
    fn wrap_empty_then_filled_line_cursor_on_empty() {
        let (rows, cr, cc) = wrap_input(&lines(&["", "abc"]), 0, 0, 3);
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].char_start, rows[0].char_end), (0, 0));
        assert_eq!((rows[1].char_start, rows[1].char_end), (0, 3));
        assert_eq!((cr, cc), (0, 0));
    }

    #[test]
    fn wrap_width_one_degenerate() {
        let (rows, cr, cc) = wrap_input(&lines(&["abc"]), 0, 2, 1);
        assert_eq!(rows.len(), 3);
        assert_eq!((cr, cc), (2, 0));
    }
}
