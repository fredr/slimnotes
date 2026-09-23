//! Standalone harness for the editor. Run: `cargo run -p slimnotes-editor --example demo`

use std::path::PathBuf;

use gpui::{px, size, AppContext, Application, WindowBounds, WindowOptions};
use slimnotes_editor::MarkdownEditor;

fn sample() -> String {
    String::from(
        "# Live-preview editor\n\
         \n\
         A **live** paragraph with *inline* `styling` and a [link](https://obsidian.md).\n\
         \n\
         ## Tasks\n\
         \n\
         - [x] build the checkbox widget\n\
         - [x] render bullet glyphs\n\
         - [ ] click a box to toggle it\n\
         - [ ] ship it\n\
         \n\
         ## A plain list\n\
         \n\
         - apples\n\
         - pears\n\
         - a much longer bullet item that should soft-wrap across several visual rows so we can confirm the hanging text still lines up after the bullet glyph\n\
         \n\
         ## Some code\n\
         \n\
         ```rust\n\
         fn main() {\n\
             println!(\"hello, panel\");\n\
         }\n\
         ```\n\
         \n\
         ## An image\n\
         \n\
         ![an orange test image](pic.png)\n\
         \n\
         And an Obsidian embed resolved from an attachments folder:\n\
         \n\
         ![[att.png]]\n\
         \n\
         #### A level-4 heading\n\
         \n\
         Wikilinks: see [[Some Note]] and [[Projects/Roadmap|the roadmap]].\n\
         Some ==highlighted text== and a #project tag inline.\n\
         \n\
         1. first ordered item\n\
         2. a much longer ordered item that should soft-wrap across several visual rows to confirm the number hangs like a bullet does\n\
         \n\
         > A blockquote line,\n\
         > spanning two lines.\n\
         \n\
         ---\n\
         \n\
         | Feature | Status | Notes |\n\
         | --- | :---: | ---: |\n\
         | Tables | done | right-aligned |\n\
         | Wikilinks | done | centered status |\n\
         \n\
         | Topic | Detail | Owner |\n\
         | --- | --- | --- |\n\
         | Wrapping | A cell whose text is far too long to sit on one row, so the column has to give way and the text has to wrap inside the cell instead of running off the right edge of the window | someone with a rather long name |\n\
         | Short | ok | me |\n\
         \n\
         Back to normal prose after the block.",
    )
}

fn main() {
    Application::new().run(|cx| {
        let bounds = WindowBounds::centered(size(px(720.), px(560.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(bounds),
                ..Default::default()
            },
            |window, cx| {
                let base_dir = std::env::var_os("SLIMNOTES_DEMO_DIR").map(PathBuf::from);
                let vault_dir = base_dir.clone();
                let text = sample();
                // Optionally open with the caret placed just after a substring,
                // to show cursor-dependent reveal (code fences, checkbox, etc.).
                let caret = std::env::var("SLIMNOTES_DEMO_CURSOR")
                    .ok()
                    .and_then(|needle| text.find(&needle).map(|i| i + needle.len()));
                cx.new(|cx| {
                    let mut ed = MarkdownEditor::new(text, base_dir, vault_dir, window, cx);
                    if let Some(c) = caret {
                        ed.place_cursor(c);
                    }
                    ed
                })
            },
        )
        .expect("failed to open window");
    });
}
