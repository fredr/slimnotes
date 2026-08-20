//! A large synthetic note, for perf work and render-regression testing.
//!
//! Run: `cargo run -p slimnotes-editor --example bigdoc`
//!
//! The content is generated deterministically (no RNG, no files read), so two
//! runs render byte-identically and screenshots can be diffed pixel-for-pixel.
//! It deliberately mixes every construct whose height is non-trivial: soft-
//! wrapping paragraphs (multi-row lines), differently-sized headings, fenced
//! code, tables, lists, and blockquotes.

use std::path::PathBuf;

use gpui::{px, size, AppContext, Application, WindowBounds, WindowOptions};
use slimnotes_editor::MarkdownEditor;

/// Number of sections; each contributes ~20 lines.
const SECTIONS: usize = 100;

fn sample() -> String {
    let mut s = String::from("# Big synthetic document\n\nA fixed, generated note used to exercise layout at scale.\n");
    for i in 0..SECTIONS {
        s.push_str(&format!("\n## Section {i}\n\n"));
        // A long paragraph that soft-wraps across several visual rows, with
        // inline markup so concealment/reveal is in play on a wrapped line.
        s.push_str(&format!(
            "Paragraph {i} with **bold text**, *italics*, `inline_code_{i}`, a [link](https://example.invalid/{i}) \
             and enough additional words to guarantee that this line soft-wraps across at least two or three \
             visual rows at any sensible window width, which is what makes per-line height interesting.\n\n"
        ));
        s.push_str(&format!("- first bullet of section {i}\n"));
        s.push_str(&format!("- second bullet of section {i} which is long enough to wrap onto another visual row as well\n"));
        s.push_str(&format!("- [ ] an unchecked task in section {i}\n"));
        s.push_str(&format!("- [x] a completed task in section {i}\n\n"));
        s.push_str(&format!("> A blockquote in section {i},\n> spanning two lines.\n\n"));
        s.push_str("```rust\n");
        s.push_str(&format!("fn section_{i}() -> usize {{\n    let value = {i};\n    value * 2\n}}\n"));
        s.push_str("```\n\n");
        s.push_str("| Key | Value | Note |\n| --- | :---: | ---: |\n");
        s.push_str(&format!("| alpha | {i} | first |\n| beta | {} | second |\n\n", i * 2));
        s.push_str(&format!("Closing prose for section {i}.\n"));
    }
    s
}

fn main() {
    Application::new().run(|cx| {
        let text = sample();
        eprintln!(
            "bigdoc: {} lines, {} bytes",
            text.split('\n').count(),
            text.len()
        );
        let bounds = WindowBounds::centered(size(px(900.), px(700.)), cx);
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
