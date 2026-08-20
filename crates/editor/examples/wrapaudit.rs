//! Does the cheap `LineWrapper` agree with full `shape_text` on the number of
//! wrapped ROWS? Row count is all a virtualized height table needs.
//!
//! Run: `cargo run -p slimnotes-editor --example wrapaudit`
//!
//! It opens a 1x1 window (required: `shape_text` lives on `WindowTextSystem`,
//! whose constructor is private), prints a table, and exits immediately.
//!
//! Why it matters: `LineWrapper` measures with ONE font, while `shape_text`
//! wraps using the real per-run glyph advances (bold/italic/mono are wider).
//! gpui itself duplicates the wrapping algorithm for the two paths, noting
//! "there are some differences" — so agreement has to be measured, not assumed.

use std::borrow::Cow;

use gpui::{
    px, size, Application, Bounds, Font, FontFeatures, FontStyle, FontWeight,
    LineFragment, Pixels, SharedString, TextRun, WindowBounds, WindowOptions,
};

fn font(family: &'static str, weight: FontWeight, style: FontStyle) -> Font {
    Font {
        family: family.into(),
        features: FontFeatures::default(),
        weight,
        style,
        fallbacks: None,
    }
}

fn main() {
    Application::new().run(|cx| {
        let faces: Vec<Cow<'static, [u8]>> = vec![
            Cow::Borrowed(&include_bytes!("../fonts/IBMPlexSans-Regular.ttf")[..]),
            Cow::Borrowed(&include_bytes!("../fonts/IBMPlexSans-Italic.ttf")[..]),
            Cow::Borrowed(&include_bytes!("../fonts/IBMPlexSans-SemiBold.ttf")[..]),
            Cow::Borrowed(&include_bytes!("../fonts/IBMPlexSans-Bold.ttf")[..]),
            Cow::Borrowed(&include_bytes!("../fonts/IBMPlexSans-BoldItalic.ttf")[..]),
            Cow::Borrowed(&include_bytes!("../fonts/IBMPlexMono-Regular.ttf")[..]),
            Cow::Borrowed(&include_bytes!("../fonts/IBMPlexMono-Italic.ttf")[..]),
            Cow::Borrowed(&include_bytes!("../fonts/IBMPlexMono-Bold.ttf")[..]),
            Cow::Borrowed(&include_bytes!("../fonts/IBMPlexMono-BoldItalic.ttf")[..]),
        ];
        cx.text_system().add_fonts(faces).expect("fonts");

        let opts = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: gpui::point(px(0.), px(0.)),
                size: size(px(1.), px(1.)),
            })),
            ..Default::default()
        };
        cx.open_window(opts, |window, cx| -> gpui::Entity<gpui::EmptyView> {
            let sans = font("IBM Plex Sans", FontWeight::NORMAL, FontStyle::Normal);
            let bold = font("IBM Plex Sans", FontWeight::BOLD, FontStyle::Normal);
            let ital = font("IBM Plex Sans", FontWeight::NORMAL, FontStyle::Italic);
            let mono = font("IBM Plex Mono", FontWeight::NORMAL, FontStyle::Normal);
            let size_px: Pixels = px(18.);

            let run = |f: &Font, len: usize| TextRun {
                len,
                font: f.clone(),
                color: gpui::black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };

            let mut cases: Vec<(&str, String, Vec<TextRun>)> = Vec::new();

            let t1 = "Paragraph 7 with plain prose and enough additional words to guarantee that \
                      this line soft-wraps across at least two or three visual rows at any \
                      sensible window width, which is what makes per-line height interesting."
                .to_string();
            let r1 = vec![run(&sans, t1.len())];
            cases.push(("uniform-sans", t1, r1));

            // Mirrors a bigdoc paragraph: regular + bold + italic + inline code.
            let (a, b, c, d, e, f) =
                ("Paragraph 7 with ", "bold text", ", ", "italics", ", ", "inline_code_7");
            let g = " and enough additional words to guarantee that this line soft-wraps across \
                     at least two or three visual rows at any sensible window width, which is \
                     what makes per-line height interesting.";
            let t2 = format!("{a}{b}{c}{d}{e}{f}{g}");
            let r2 = vec![
                run(&sans, a.len()),
                run(&bold, b.len()),
                run(&sans, c.len()),
                run(&ital, d.len()),
                run(&sans, e.len()),
                run(&mono, f.len()),
                run(&sans, g.len()),
            ];
            cases.push(("mixed-fonts", t2, r2));

            let h = "A line where ";
            let i = "a very large proportion of the visible text is set in bold weight and \
                     therefore measurably wider than the regular face the cheap wrapper uses";
            let j = " plus a short tail.";
            let t3 = format!("{h}{i}{j}");
            let r3 = vec![run(&sans, h.len()), run(&bold, i.len()), run(&sans, j.len())];
            cases.push(("bold-heavy", t3, r3));

            let k = "Config: ";
            let l = "some_module::function_name(argument_one, argument_two, arg_three) -> Result<T>";
            let t4 = format!("{k}{l}");
            let r4 = vec![run(&sans, k.len()), run(&mono, l.len())];
            cases.push(("mono-heavy", t4, r4));

            let t5 = "Här är en längre svensk mening med å ä ö och é som ska radbrytas över \
                      flera visuella rader för att testa breddberäkningen ordentligt nog."
                .to_string();
            let r5 = vec![run(&sans, t5.len())];
            cases.push(("swedish", t5, r5));

            let mut total = 0usize;
            let mut mism = 0usize;
            println!("{:<14} {:>6} {:>8}  examples (width: cheap vs shaped)", "case", "widths", "mismatch");
            for (label, text, runs) in &cases {
                let shared = SharedString::from(text.clone());
                let mut bad = Vec::new();
                let mut n = 0usize;
                for w in (300..=1000).step_by(10) {
                    let width = px(w as f32);
                    n += 1;
                    total += 1;

                    let cheap = {
                        let mut wrapper =
                            cx.text_system().line_wrapper(sans.clone(), size_px);
                        wrapper.wrap_line(&[LineFragment::text(text)], width).count() + 1
                    };
                    let truth: usize = window
                        .text_system()
                        .shape_text(shared.clone(), size_px, runs, Some(width), None)
                        .map(|ls| ls.iter().map(|l| l.wrap_boundaries.len() + 1).sum())
                        .unwrap_or(0);

                    if cheap != truth {
                        mism += 1;
                        if bad.len() < 5 {
                            bad.push((w, cheap, truth));
                        }
                    }
                }
                println!("{label:<14} {n:>6} {:>8}  {:?}", bad.len(), bad);
            }
            println!("\nTOTAL {total} comparisons, {mism} mismatches");

            // Second question: is "sum of per-run char advances" a safe UPPER
            // bound on the shaped width? If so, a line whose bound fits the wrap
            // width provably needs exactly one row — an exact, shaping-free fast
            // path for the (many) short lines, with long lines still shaped.
            println!("\n-- advance-sum bound vs shaped width --");
            let mut worst = 0.0f32;
            let mut unsafe_cases = 0usize;
            for (label, text, runs) in &cases {
                let shared = SharedString::from(text.clone());
                let shaped_w =
                    f32::from(window.text_system().shape_line(shared, size_px, runs, None).width);
                // Sum advances per run, each in that run's own font.
                let mut bound = 0.0f32;
                let mut pos = 0usize;
                for r in runs {
                    let seg = &text[pos..pos + r.len];
                    let fid = cx.text_system().resolve_font(&r.font);
                    for ch in seg.chars() {
                        bound += cx
                            .text_system()
                            .advance(fid, size_px, ch)
                            .map(|s| f32::from(s.width))
                            .unwrap_or(0.);
                    }
                    pos += r.len;
                }
                let ratio = shaped_w / bound;
                if ratio > worst {
                    worst = ratio;
                }
                if shaped_w > bound {
                    unsafe_cases += 1;
                }
                println!(
                    "{label:<14} shaped={shaped_w:8.2} bound={bound:8.2} shaped/bound={ratio:.4}"
                );
            }
            // Substituted glyphs the editor actually puts in display strings
            // (bullet, checkbox em-space) plus an emoji that needs a fallback
            // font — the cases where a per-font advance could underestimate.
            println!("\n-- substituted / fallback glyphs --");
            for (label, s) in [
                ("bullet", "\u{2022} a bulleted list item that is reasonably long"),
                ("checkbox", "\u{2003} a task item that is reasonably long here"),
                ("emoji", "crab \u{1F980} and text after it here"),
                ("cjk", "hello \u{4F60}\u{597D}\u{4E16}\u{754C} world"),
            ] {
                let runs = vec![run(&sans, s.len())];
                let shaped_w = f32::from(
                    window
                        .text_system()
                        .shape_line(SharedString::from(s.to_string()), size_px, &runs, None)
                        .width,
                );
                let fid = cx.text_system().resolve_font(&sans);
                let mut bound = 0.0f32;
                let mut errs = 0;
                for ch in s.chars() {
                    match cx.text_system().advance(fid, size_px, ch) {
                        Ok(a) => bound += f32::from(a.width),
                        Err(_) => errs += 1,
                    }
                }
                println!(
                    "{label:<9} shaped={shaped_w:8.2} bound={bound:8.2} ratio={:.4} advance_errs={errs} {}",
                    shaped_w / bound,
                    if shaped_w <= bound { "ok" } else { "UNDERESTIMATE" }
                );
            }

            println!(
                "\nworst shaped/bound = {worst:.4}; bound violated in {unsafe_cases} case(s) -> {}",
                if unsafe_cases == 0 && worst <= 1.0 {
                    "SAFE upper bound"
                } else {
                    "NOT a safe bound"
                }
            );
            println!(
                "VERDICT: {}",
                if mism == 0 {
                    "row counts AGREE — wrapper-based exact heights are viable"
                } else {
                    "row counts DIFFER — wrapper heights would drift the scrollbar"
                }
            );
            std::process::exit(0);
        })
        .expect("window");
    });
}
