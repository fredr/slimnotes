//! A wrap-heavy note, for chasing missing or misplaced text.
//!
//! Unlike `bigdoc`, most lines here span several visual rows and some contain a
//! single unbreakable token (a long URL) that the shaper must break mid-word —
//! the regime where a line's height cannot be known without shaping it.
//!
//! `SLIMNOTES_NOTE=<path>` renders that file instead, which is the quickest way
//! to reproduce a rendering problem against real content without going through
//! the app (pair it with `SLIMNOTES_CHECK_SHAPES=1`).

use std::path::PathBuf;

use gpui::{px, size, AppContext, Application, WindowBounds, WindowOptions};
use slimnotes_editor::MarkdownEditor;

const SECTIONS: usize = 120;

fn sample() -> String {
    let mut s = String::from("# Wrap torture test\n\n");
    for i in 0..SECTIONS {
        s.push_str(&format!("## Section {i}\n\n"));
        // A bare long URL: one token, no break opportunities.
        s.push_str(&format!(
            "Pointer types discussion for null: https://github.com/encoredev/encore/pull/{i}#discussion_r98430204{i}\n\n"
        ));
        // Prose that wraps to several rows.
        s.push_str(&format!(
            "Section {i} prose that keeps going for long enough to occupy several visual rows at \
             any reasonable window width, mixing **bold**, *italics* and `code_{i}` so that reveal \
             has spans to work with while the line is wrapped across those rows.\n\n"
        ));
        // A wrapped list item (hanging indent re-shapes at a narrower width).
        s.push_str(&format!(
            "- a list item in section {i} that is itself long enough to wrap onto continuation rows, \
             which are hang-indented under the text rather than the bullet\n\n"
        ));
        s.push_str("```python\n");
        s.push_str(&format!("settings = dap[\"Delve\"]\nuser_install_path = settings[\"binary_{i}\"]\nuser_args = settings[\"args\"]\n"));
        s.push_str("```\n\n");
    }
    s
}

fn main() {
    Application::new().run(|cx| {
        // `SLIMNOTES_NOTE=<path>` loads a note instead of the generated one.
        let text = match std::env::var_os("SLIMNOTES_NOTE") {
            Some(p) => std::fs::read_to_string(p).expect("failed to read SLIMNOTES_NOTE"),
            None => sample(),
        };
        eprintln!(
            "wraptest: {} lines, {} bytes",
            text.split('\n').count(),
            text.len()
        );
        let bounds = WindowBounds::centered(size(px(700.), px(700.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(bounds),
                ..Default::default()
            },
            |window, cx| {
                let base: Option<PathBuf> = None;
                cx.new(|cx| MarkdownEditor::new(text, base.clone(), base, window, cx))
            },
        )
        .expect("failed to open window");
    });
}
