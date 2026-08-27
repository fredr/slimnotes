//! Walks the caret down (then back up) through a document whose lines soft-wrap,
//! checking that every press moves it exactly one visual row.
//!
//! Vertical motion is the one thing about the editor that can't be unit-tested:
//! it is computed from the painted layout, so it needs a real window and real
//! shaping. This example supplies both — it opens a window, dispatches synthetic
//! `down`/`up` keystrokes, and exits non-zero if the caret ever stalls or the
//! two directions disagree. Run it after touching the layout or the caret:
//!
//! ```text
//! cargo run -p slimnotes-editor --example keynav
//! ```
//!
//! It caught this: on a wrapped line the caret landed on a wrap boundary, whose
//! reported position is the *end of the previous row*, so the next `down` aimed
//! at the row it was already on and the caret could never leave the line.
//!
//! Passing keystrokes as arguments drives it by hand instead (`N:key` repeats):
//! `cargo run --example keynav -- 3:down 60:right 12:down`.

use std::path::PathBuf;
use std::time::Duration;

use gpui::{px, size, AppContext, Application, Keystroke, WindowBounds, WindowOptions};
use slimnotes_editor::MarkdownEditor;

/// Short lines around three kinds of wrapped line: a paragraph, a bullet item
/// and an ordered item (the list items hang-indent their continuation rows, so
/// their rows don't all start at the same x).
fn sample() -> String {
    String::from(
        "short one\n\
         \n\
         A long paragraph that is quite certainly going to soft-wrap across at least three visual rows at this window width, because it just keeps going and going with plenty of ordinary words in it so the shaper has many break opportunities to choose from as it fills each row to the edge.\n\
         \n\
         - a bullet item long enough to wrap onto two or three continuation rows, which are hang-indented under the text rather than under the bullet glyph itself, so the x offsets differ per row.\n\
         \n\
         1. an ordered item that also wraps across a few rows so the numbering hangs the same way the bullet does when the row is refilled.\n\
         \n\
         short two\n\
         short three\n",
    )
}

/// Presses to make in the self-checking run: enough `down` to reach the end of
/// the sample from the top, whatever its rows work out to at this width.
const PRESSES: usize = 20;

/// Dispatch one keystroke, let the frame it triggers paint, and report where the
/// caret ended up. The paint matters: vertical motion is computed from the last
/// painted layout, so reading the caret before the repaint would test nothing.
async fn press(
    cx: &mut gpui::AsyncApp,
    window: gpui::AnyWindowHandle,
    editor: &gpui::Entity<MarkdownEditor>,
    key: &str,
) -> usize {
    let _ = cx.update_window(window, |_root, window, cx| {
        window.dispatch_keystroke(Keystroke::parse(key).expect("bad keystroke"), cx);
    });
    cx.background_executor().timer(Duration::from_millis(120)).await;
    editor.read_with(cx, |e, _| e.cursor()).expect("editor is gone")
}

fn main() {
    // `N:key` repeats a keystroke N times.
    let keys: Vec<String> = std::env::args()
        .skip(1)
        .flat_map(|k| match k.split_once(':') {
            Some((n, k)) => vec![k.to_string(); n.parse().expect("N:key needs a number")],
            None => vec![k],
        })
        .collect();
    Application::new().run(move |cx| {
        let bounds = WindowBounds::centered(size(px(700.), px(700.)), cx);
        let window = cx
            .open_window(
                WindowOptions { window_bounds: Some(bounds), ..Default::default() },
                |window, cx| {
                    let base: Option<PathBuf> = None;
                    cx.new(|cx| MarkdownEditor::new(sample(), base.clone(), base, window, cx))
                },
            )
            .expect("failed to open window");
        let editor = window.entity(cx).expect("window has no editor");
        cx.spawn(async move |cx| {
            // One frame has to be painted before the first keystroke: vertical
            // motion reads the layout that paint produces.
            cx.background_executor().timer(Duration::from_millis(700)).await;

            let mut failures = 0;
            if keys.is_empty() {
                let end = editor.read_with(cx, |e, _| e.text().len()).expect("editor is gone");
                // Down to the end of the document, recording every stop.
                let mut down = vec![0usize];
                let mut reached_end = false;
                for i in 0..PRESSES {
                    let at = press(cx, window.into(), &editor, "down").await;
                    let last = *down.last().unwrap();
                    if at == end {
                        println!("down {i:2}: {last} -> {at} (document end)");
                        reached_end = true;
                        if at != last {
                            down.push(at);
                        }
                        break;
                    }
                    println!("down {i:2}: {last} -> {at}");
                    if at <= last {
                        println!("  FAIL: down did not move the caret forward");
                        failures += 1;
                        break;
                    }
                    down.push(at);
                }
                if !reached_end && failures == 0 {
                    println!("  FAIL: {PRESSES} presses did not reach the document end");
                    failures += 1;
                }
                // And back up: the same offsets in reverse, since every stop was
                // one visual row and rows don't move.
                let presses = down.len() - 1;
                for i in 0..presses {
                    let at = press(cx, window.into(), &editor, "up").await;
                    let want = down[presses - 1 - i];
                    println!("up   {i:2}: -> {at} (want {want})");
                    if at != want {
                        println!("  FAIL: up did not retrace down");
                        failures += 1;
                        break;
                    }
                }
            } else {
                for k in &keys {
                    let at = press(cx, window.into(), &editor, k).await;
                    println!("{k}: -> {at}");
                }
            }
            println!(
                "{}",
                if failures == 0 { "keynav: ok" } else { "keynav: FAILED" }
            );
            let _ = cx.update(|cx| cx.quit());
            if failures > 0 {
                std::process::exit(1);
            }
        })
        .detach();
    });
}
