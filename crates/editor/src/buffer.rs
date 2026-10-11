use ropey::Rope;
use std::collections::VecDeque;
use std::ops::Range;

/// Maximum number of undo entries retained. Each entry stores only the changed
/// span (removed + inserted text), not a full-document clone, so this cap is
/// cheap regardless of document size.
const MAX_UNDO: usize = 300;

/// An inverse-able edit delta.
///
/// The forward edit replaced the byte span `start..start+removed.len()` with
/// `inserted`. Therefore:
/// - to UNDO: replace `start..start+inserted.len()` with `removed`, restore `cursor_before`.
/// - to REDO: replace `start..start+removed.len()` with `inserted`, restore `cursor_after`.
#[derive(Clone, Debug)]
struct Delta {
    start: usize,
    removed: String,
    inserted: String,
    cursor_before: usize,
    cursor_after: usize,
}

/// A text buffer backed by a `ropey::Rope` with delta-based undo/redo.
///
/// All public byte offsets are BYTE offsets into the UTF-8 document and are
/// guarded against landing inside a multi-byte character.
#[derive(Clone)]
pub struct Buffer {
    rope: Rope,
    undo: VecDeque<Delta>,
    redo: Vec<Delta>,
    version: u64,
    /// Coalesce key of the current (top-of-undo) edit group, or `None` if the
    /// next edit must start a fresh group.
    last_coalesce: Option<u8>,
}

// Some accessors (byte<->line math, slicing, boundary helpers) are the buffer's
// public API for the layout/virtualization work and aren't all wired up yet.
#[allow(dead_code)]
impl Buffer {
    pub fn new(text: &str) -> Self {
        Buffer {
            rope: Rope::from_str(text),
            undo: VecDeque::new(),
            redo: Vec::new(),
            version: 0,
            last_coalesce: None,
        }
    }

    /// Monotonic; bumps on every mutation (insert/delete/replace/undo/redo/set_text).
    /// The editor keys its parse/layout caches on this.
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn is_empty(&self) -> bool {
        self.rope.len_bytes() == 0
    }

    pub fn len_bytes(&self) -> usize {
        self.rope.len_bytes()
    }

    /// Full document text. The editor memoizes its parse by `version`, so this
    /// is materialized at most once per edit.
    pub fn text(&self) -> String {
        self.rope.to_string()
    }

    /// Substring by byte range (clamped to valid char boundaries).
    pub fn slice(&self, range: Range<usize>) -> String {
        let start = self.clamp_boundary(range.start);
        let end = self.clamp_boundary(range.end);
        if start >= end {
            return String::new();
        }
        self.rope.byte_slice(start..end).to_string()
    }

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// 0-based line containing `byte` (a byte at a '\n' belongs to the line it ends).
    pub fn byte_to_line(&self, byte: usize) -> usize {
        let byte = self.clamp_boundary(byte);
        self.rope.byte_to_line(byte)
    }

    /// Byte offset of the start of 0-based `line`.
    pub fn line_to_byte(&self, line: usize) -> usize {
        let line = line.min(self.rope.len_lines());
        self.rope.line_to_byte(line)
    }

    /// The text of 0-based `line`, WITHOUT the trailing newline.
    pub fn line(&self, line: usize) -> String {
        if line >= self.rope.len_lines() {
            return String::new();
        }
        let full = self.rope.line(line).to_string();
        let trimmed = full
            .strip_suffix('\n')
            .map(|t| t.strip_suffix('\r').unwrap_or(t))
            .unwrap_or(&full);
        trimmed.to_string()
    }

    // ---- UTF-16 conversions ----
    //
    // The platform text-input protocol (IME) addresses the document in UTF-16
    // code units, while everything else here uses byte offsets. These convert
    // between the two via the rope's char index, so they stay O(log n) and can
    // never land inside a character.

    /// Byte offset -> UTF-16 code-unit offset.
    pub fn byte_to_utf16(&self, byte: usize) -> usize {
        let ch = self.rope.byte_to_char(byte.min(self.rope.len_bytes()));
        self.rope.char_to_utf16_cu(ch)
    }

    /// UTF-16 code-unit offset -> byte offset (clamped to the document).
    pub fn utf16_to_byte(&self, cu: usize) -> usize {
        let ch = self.rope.utf16_cu_to_char(cu.min(self.rope.len_utf16_cu()));
        self.rope.char_to_byte(ch)
    }

    /// Document length in UTF-16 code units.
    pub fn len_utf16(&self) -> usize {
        self.rope.len_utf16_cu()
    }

    /// Nearest char boundary <= byte (for safe cursor placement).
    pub fn clamp_boundary(&self, byte: usize) -> usize {
        let byte = byte.min(self.rope.len_bytes());
        // `byte_to_char` rounds a non-boundary byte down to the char that
        // contains it; `char_to_byte` then yields that char's start byte.
        let ch = self.rope.byte_to_char(byte);
        self.rope.char_to_byte(ch)
    }

    /// Char before `byte`, or 0.
    pub fn prev_boundary(&self, byte: usize) -> usize {
        let b = self.clamp_boundary(byte);
        if b == 0 {
            return 0;
        }
        let ch = self.rope.byte_to_char(b);
        self.rope.char_to_byte(ch - 1)
    }

    /// Char after `byte`, or len.
    pub fn next_boundary(&self, byte: usize) -> usize {
        let b = self.clamp_boundary(byte);
        if b >= self.rope.len_bytes() {
            return self.rope.len_bytes();
        }
        let ch = self.rope.byte_to_char(b);
        self.rope.char_to_byte(ch + 1)
    }

    // --- editing ---

    pub fn replace(
        &mut self,
        range: Range<usize>,
        text: &str,
        cursor_before: usize,
        coalesce: Option<u8>,
    ) -> usize {
        self.replace_caret(range, text, text.len(), cursor_before, coalesce)
    }

    /// [`replace`](Self::replace), with the caret landing `caret` bytes into
    /// `text` rather than after all of it — for an edit that also rewrites text
    /// past where the caret belongs. Redo puts it back there too.
    pub fn replace_caret(
        &mut self,
        range: Range<usize>,
        text: &str,
        caret: usize,
        cursor_before: usize,
        coalesce: Option<u8>,
    ) -> usize {
        let start = self.clamp_boundary(range.start);
        let mut end = self.clamp_boundary(range.end);
        if end < start {
            end = start;
        }

        let removed = if end > start {
            self.rope.byte_slice(start..end).to_string()
        } else {
            String::new()
        };

        // No-op guard: nothing removed, nothing inserted.
        if removed.is_empty() && text.is_empty() {
            return start;
        }

        self.apply(start, end, text);

        let cursor_after = start + caret.min(text.len());

        let delta = Delta {
            start,
            removed,
            inserted: text.to_string(),
            cursor_before,
            cursor_after,
        };
        self.record(delta, coalesce);
        self.version += 1;
        cursor_after
    }

    pub fn insert(
        &mut self,
        at: usize,
        text: &str,
        cursor_before: usize,
        coalesce: Option<u8>,
    ) -> usize {
        self.replace(at..at, text, cursor_before, coalesce)
    }

    pub fn delete(
        &mut self,
        range: Range<usize>,
        cursor_before: usize,
        coalesce: Option<u8>,
    ) -> usize {
        self.replace(range, "", cursor_before, coalesce)
    }

    /// Undo the most recent edit group. Returns the caret byte offset to
    /// restore, or `None` if there is nothing to undo.
    pub fn undo(&mut self) -> Option<usize> {
        let d = self.undo.pop_back()?;
        let start = d.start;
        let end = start + d.inserted.len();
        self.apply(start, end, &d.removed);
        let cursor = d.cursor_before;
        self.redo.push(d);
        self.version += 1;
        self.last_coalesce = None;
        Some(cursor)
    }

    /// Redo the most recently undone edit group. Returns the caret byte offset
    /// to restore, or `None` if there is nothing to redo.
    pub fn redo(&mut self) -> Option<usize> {
        let d = self.redo.pop()?;
        let start = d.start;
        let end = start + d.removed.len();
        self.apply(start, end, &d.inserted);
        let cursor = d.cursor_after;
        self.undo.push_back(d);
        self.version += 1;
        self.last_coalesce = None;
        Some(cursor)
    }

    /// Replace the whole buffer (opening a different note); clears undo/redo,
    /// bumps version.
    pub fn set_text(&mut self, text: &str) {
        self.rope = Rope::from_str(text);
        self.undo.clear();
        self.redo.clear();
        self.last_coalesce = None;
        self.version += 1;
    }

    // --- internals ---

    /// Apply a byte-range replacement to the rope. `start`/`end` MUST be valid
    /// char boundaries.
    fn apply(&mut self, start: usize, end: usize, text: &str) {
        let cs = self.rope.byte_to_char(start);
        let ce = self.rope.byte_to_char(end);
        if ce > cs {
            self.rope.remove(cs..ce);
        }
        if !text.is_empty() {
            self.rope.insert(cs, text);
        }
    }

    /// Record a delta into the undo stack, coalescing into the previous entry
    /// when the key matches and the edits are contiguous. Always clears redo.
    fn record(&mut self, delta: Delta, coalesce: Option<u8>) {
        self.redo.clear();

        if let Some(k) = coalesce {
            if self.last_coalesce == Some(k) {
                if let Some(prev) = self.undo.back_mut() {
                    if Self::try_merge(prev, &delta) {
                        return;
                    }
                }
            }
        }

        if self.undo.len() >= MAX_UNDO {
            self.undo.pop_front();
        }
        self.undo.push_back(delta);
        self.last_coalesce = coalesce;
    }

    /// Try to fold `d` into `prev` (a contiguous edit in the same group).
    /// Returns true if merged.
    fn try_merge(prev: &mut Delta, d: &Delta) -> bool {
        // Typing: an insertion appended immediately after prev's inserted text.
        if d.removed.is_empty()
            && !d.inserted.is_empty()
            && d.start == prev.start + prev.inserted.len()
        {
            prev.inserted.push_str(&d.inserted);
            prev.cursor_after = d.cursor_after;
            return true;
        }

        // Backspace: a deletion whose span ends exactly at prev's start.
        if d.inserted.is_empty()
            && !d.removed.is_empty()
            && prev.inserted.is_empty()
            && d.start + d.removed.len() == prev.start
        {
            prev.start = d.start;
            let mut merged = d.removed.clone();
            merged.push_str(&prev.removed);
            prev.removed = merged;
            prev.cursor_after = d.cursor_after;
            return true;
        }

        // Forward delete: repeated deletion at the same start.
        if d.inserted.is_empty()
            && !d.removed.is_empty()
            && prev.inserted.is_empty()
            && d.start == prev.start
        {
            prev.removed.push_str(&d.removed);
            prev.cursor_after = d.cursor_after;
            return true;
        }

        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_updates_text_and_caret_and_version() {
        let mut b = Buffer::new("hello");
        let v0 = b.version();
        let caret = b.insert(5, " world", 5, None);
        assert_eq!(b.text(), "hello world");
        assert_eq!(caret, 11);
        assert!(b.version() > v0);
    }

    #[test]
    fn insert_in_middle() {
        let mut b = Buffer::new("held");
        let caret = b.insert(2, "l", 2, None);
        assert_eq!(b.text(), "helld");
        assert_eq!(caret, 3);
    }

    #[test]
    fn delete_updates_text_and_caret() {
        let mut b = Buffer::new("hello world");
        let caret = b.delete(5..11, 11, None);
        assert_eq!(b.text(), "hello");
        assert_eq!(caret, 5);
    }

    #[test]
    fn replace_updates_text_and_caret() {
        let mut b = Buffer::new("hello world");
        let caret = b.replace(6..11, "there", 11, None);
        assert_eq!(b.text(), "hello there");
        assert_eq!(caret, 11);
    }

    #[test]
    fn basic_accessors() {
        let b = Buffer::new("abc");
        assert!(!b.is_empty());
        assert_eq!(b.len_bytes(), 3);
        assert_eq!(b.slice(0..2), "ab");
        assert!(Buffer::new("").is_empty());
    }

    #[test]
    fn slice_clamps() {
        let b = Buffer::new("héllo"); // é is 2 bytes
        // full text bytes: h(0) é(1..3) l(3) l(4) o(5) => len 6
        assert_eq!(b.len_bytes(), 6);
        // slice with a mid-char end clamps down to the char boundary.
        assert_eq!(b.slice(0..2), "h");
        assert_eq!(b.slice(0..3), "hé");
    }

    #[test]
    fn byte_line_roundtrip_multiline() {
        let b = Buffer::new("one\ntwo\nthree");
        // line_to_byte
        assert_eq!(b.line_to_byte(0), 0);
        assert_eq!(b.line_to_byte(1), 4);
        assert_eq!(b.line_to_byte(2), 8);
        // byte_to_line
        assert_eq!(b.byte_to_line(0), 0);
        assert_eq!(b.byte_to_line(3), 0); // the '\n' ending line 0
        assert_eq!(b.byte_to_line(4), 1);
        assert_eq!(b.byte_to_line(8), 2);
        assert_eq!(b.byte_to_line(13), 2); // end of buffer
        // line text (no trailing newline)
        assert_eq!(b.line(0), "one");
        assert_eq!(b.line(1), "two");
        assert_eq!(b.line(2), "three");
        assert_eq!(b.line_count(), 3);
    }

    #[test]
    fn empty_buffer_lines() {
        let b = Buffer::new("");
        assert_eq!(b.line_count(), 1);
        assert_eq!(b.byte_to_line(0), 0);
        assert_eq!(b.line_to_byte(0), 0);
        assert_eq!(b.line(0), "");
    }

    #[test]
    fn trailing_newline() {
        let b = Buffer::new("a\n");
        assert_eq!(b.line_count(), 2);
        assert_eq!(b.line(0), "a");
        assert_eq!(b.line(1), "");
        // byte at the '\n' belongs to line 0; the byte after belongs to line 1.
        assert_eq!(b.byte_to_line(1), 0);
        assert_eq!(b.byte_to_line(2), 1);
        assert_eq!(b.line_to_byte(1), 2);
    }

    #[test]
    fn multibyte_boundaries() {
        // "a→b": a(0) →(1..4, 3 bytes) b(4) => len 5
        let b = Buffer::new("a→b");
        assert_eq!(b.len_bytes(), 5);
        // clamp mid-char rounds down to the char start (byte 1).
        assert_eq!(b.clamp_boundary(2), 1);
        assert_eq!(b.clamp_boundary(3), 1);
        assert_eq!(b.clamp_boundary(4), 4);
        // next/prev boundary step whole chars.
        assert_eq!(b.next_boundary(1), 4); // past the arrow
        assert_eq!(b.prev_boundary(4), 1); // back to the arrow start
        assert_eq!(b.next_boundary(0), 1);
        assert_eq!(b.prev_boundary(0), 0);
        assert_eq!(b.next_boundary(5), 5);
        // byte_to_line stays boundary-correct across the multibyte char.
        assert_eq!(b.byte_to_line(4), 0);
    }

    #[test]
    fn insert_before_multibyte_shifts_bytes() {
        let mut b = Buffer::new("é"); // 2 bytes
        let caret = b.insert(0, "x", 0, None);
        assert_eq!(b.text(), "xé");
        assert_eq!(caret, 1);
        assert_eq!(b.len_bytes(), 3);
    }

    #[test]
    fn undo_redo_restore_text_and_caret() {
        let mut b = Buffer::new("hello");
        b.insert(5, " world", 5, None);
        assert_eq!(b.text(), "hello world");
        let c = b.undo().unwrap();
        assert_eq!(b.text(), "hello");
        assert_eq!(c, 5); // cursor_before
        let c = b.redo().unwrap();
        assert_eq!(b.text(), "hello world");
        assert_eq!(c, 11); // cursor_after
    }

    #[test]
    fn replace_caret_lands_mid_insert_and_redo_returns_there() {
        let mut b = Buffer::new("1. a\n2. b");
        // Insert a new item after `a` and renumber `b` in the same edit; the
        // caret belongs after the new marker, not after the `3`.
        let c = b.replace_caret(4..6, "\n2. \n3", 4, 4, None);
        assert_eq!(b.text(), "1. a\n2. \n3. b");
        assert_eq!(c, 8);
        assert_eq!(b.undo().unwrap(), 4);
        assert_eq!(b.text(), "1. a\n2. b");
        assert_eq!(b.redo().unwrap(), 8);
        assert_eq!(b.text(), "1. a\n2. \n3. b");
    }

    #[test]
    fn undo_redo_empty_stacks() {
        let mut b = Buffer::new("x");
        assert_eq!(b.undo(), None);
        assert_eq!(b.redo(), None);
    }

    #[test]
    fn redo_cleared_by_new_edit() {
        let mut b = Buffer::new("hello");
        b.insert(5, "!", 5, None);
        b.undo();
        assert_eq!(b.text(), "hello");
        // a fresh edit must clear the redo stack
        b.insert(5, "?", 5, None);
        assert_eq!(b.redo(), None);
        assert_eq!(b.text(), "hello?");
    }

    #[test]
    fn undo_delete_and_replace() {
        let mut b = Buffer::new("hello world");
        b.delete(5..11, 11, None);
        assert_eq!(b.text(), "hello");
        assert_eq!(b.undo().unwrap(), 11);
        assert_eq!(b.text(), "hello world");

        b.replace(0..5, "HELLO", 5, None);
        assert_eq!(b.text(), "HELLO world");
        assert_eq!(b.undo().unwrap(), 5);
        assert_eq!(b.text(), "hello world");
    }

    #[test]
    fn coalesced_typing_is_one_undo_step() {
        let mut b = Buffer::new("");
        let mut caret = 0;
        for ch in ["h", "e", "l", "l", "o"] {
            caret = b.insert(caret, ch, caret, Some(1));
        }
        assert_eq!(b.text(), "hello");
        // one undo reverts all coalesced typing
        let c = b.undo().unwrap();
        assert_eq!(b.text(), "");
        assert_eq!(c, 0); // cursor_before of the very first keystroke
        assert_eq!(b.undo(), None);
        // redo brings it all back in one step too
        let c = b.redo().unwrap();
        assert_eq!(b.text(), "hello");
        assert_eq!(c, 5);
    }

    #[test]
    fn none_key_breaks_coalescing() {
        let mut b = Buffer::new("");
        let mut caret = 0;
        caret = b.insert(caret, "a", caret, Some(1));
        caret = b.insert(caret, "b", caret, None); // breaks the group
        let _ = b.insert(caret, "c", caret, Some(1));
        assert_eq!(b.text(), "abc");
        // three separate undo steps
        b.undo().unwrap();
        assert_eq!(b.text(), "ab");
        b.undo().unwrap();
        assert_eq!(b.text(), "a");
        b.undo().unwrap();
        assert_eq!(b.text(), "");
    }

    #[test]
    fn key_change_starts_new_step() {
        let mut b = Buffer::new("");
        let mut caret = 0;
        caret = b.insert(caret, "a", caret, Some(1));
        caret = b.insert(caret, "b", caret, Some(1)); // coalesces with "a"
        let _ = b.insert(caret, "c", caret, Some(2)); // different key -> new step
        assert_eq!(b.text(), "abc");
        b.undo().unwrap();
        assert_eq!(b.text(), "ab");
        b.undo().unwrap();
        assert_eq!(b.text(), "");
    }

    #[test]
    fn coalesced_backspace_is_one_step() {
        let mut b = Buffer::new("abc");
        // backspace from caret 3: delete [2,3), then [1,2), then [0,1)
        b.delete(2..3, 3, Some(2));
        b.delete(1..2, 2, Some(2));
        b.delete(0..1, 1, Some(2));
        assert_eq!(b.text(), "");
        let c = b.undo().unwrap();
        assert_eq!(b.text(), "abc");
        assert_eq!(c, 3); // cursor_before of the first backspace
    }

    #[test]
    fn coalesced_forward_delete_is_one_step() {
        let mut b = Buffer::new("abc");
        // forward-delete at caret 0 repeatedly
        b.delete(0..1, 0, Some(3));
        b.delete(0..1, 0, Some(3));
        assert_eq!(b.text(), "c");
        b.undo().unwrap();
        assert_eq!(b.text(), "abc");
    }

    #[test]
    fn undo_after_break_reverts_group() {
        let mut b = Buffer::new("");
        let mut caret = 0;
        // type "foo" (one group)
        for ch in ["f", "o", "o"] {
            caret = b.insert(caret, ch, caret, Some(1));
        }
        // then delete something with a new group
        let _ = b.delete(0..1, caret, None);
        assert_eq!(b.text(), "oo");
        b.undo().unwrap(); // reverts the delete
        assert_eq!(b.text(), "foo");
        b.undo().unwrap(); // reverts all typing at once
        assert_eq!(b.text(), "");
    }

    #[test]
    fn set_text_clears_history() {
        let mut b = Buffer::new("hello");
        b.insert(5, " world", 5, Some(1));
        let v = b.version();
        b.set_text("brand new");
        assert_eq!(b.text(), "brand new");
        assert!(b.version() > v);
        assert_eq!(b.undo(), None);
        assert_eq!(b.redo(), None);
    }

    #[test]
    fn undo_stack_capped() {
        let mut b = Buffer::new("");
        // Each edit its own group (None). Push more than the cap.
        for _ in 0..(MAX_UNDO + 50) {
            b.insert(b.len_bytes(), "x", b.len_bytes(), None);
        }
        // Should be able to undo at most MAX_UNDO times.
        let mut undos = 0;
        while b.undo().is_some() {
            undos += 1;
        }
        assert_eq!(undos, MAX_UNDO);
    }

    #[test]
    fn version_bumps_on_every_mutation() {
        let mut b = Buffer::new("abc");
        let v0 = b.version();
        b.insert(3, "d", 3, None);
        let v1 = b.version();
        assert!(v1 > v0);
        b.undo();
        let v2 = b.version();
        assert!(v2 > v1);
        b.redo();
        let v3 = b.version();
        assert!(v3 > v2);
        b.set_text("z");
        assert!(b.version() > v3);
    }

    #[test]
    fn coalesced_typing_across_multibyte() {
        let mut b = Buffer::new("");
        let mut caret = 0;
        for ch in ["a", "→", "b"] {
            caret = b.insert(caret, ch, caret, Some(1));
        }
        assert_eq!(b.text(), "a→b");
        assert_eq!(caret, 5);
        b.undo().unwrap();
        assert_eq!(b.text(), "");
    }

    #[test]
    fn utf16_offsets_round_trip() {
        // "é" is 2 bytes / 1 UTF-16 unit; "🦀" is 4 bytes / 2 UTF-16 units (a
        // surrogate pair) — the case that corrupts text if bytes and UTF-16 units
        // are confused.
        let b = Buffer::new("aé🦀b");
        assert_eq!(b.len_bytes(), 8);
        assert_eq!(b.len_utf16(), 5);

        for (byte, cu) in [(0, 0), (1, 1), (3, 2), (7, 4), (8, 5)] {
            assert_eq!(b.byte_to_utf16(byte), cu, "byte {byte} -> utf16");
            assert_eq!(b.utf16_to_byte(cu), byte, "utf16 {cu} -> byte");
        }
        // A UTF-16 offset inside the surrogate pair clamps to the char start.
        assert_eq!(b.utf16_to_byte(3), 3);
        // Out-of-range input is clamped, never a panic.
        assert_eq!(b.utf16_to_byte(999), 8);
        assert_eq!(b.byte_to_utf16(999), 5);
    }
}
