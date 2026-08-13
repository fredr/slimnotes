//! A small, *synchronous* markdown renderer: pulldown-cmark events → gpui
//! elements. Unlike gpui-component's `TextView` (which parses on a background
//! task with a 200ms debounce, causing lag and staggered rendering), this
//! builds the elements inline so a note appears instantly and all at once.
//!
//! It covers the subset slimnotes needs: headings, paragraphs, bold/italic/
//! strikethrough, inline code, links, bullet/ordered lists, blockquotes,
//! fenced code blocks and horizontal rules.

use std::ops::Range;

use gpui::{
    div, px, AnyElement, FontStyle, FontWeight, HighlightStyle, Hsla, IntoElement, ParentElement,
    SharedString, StrikethroughStyle, Styled, StyledText, UnderlineStyle,
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Colors pulled from the active theme, passed in so this module stays
/// independent of gpui-component.
#[derive(Clone, Copy)]
pub struct MdStyle {
    pub text: Hsla,
    pub muted: Hsla,
    pub code_bg: Hsla,
    pub code_fg: Hsla,
    pub link: Hsla,
    pub border: Hsla,
}

#[derive(Default, Clone, Copy)]
struct Flags {
    bold: bool,
    italic: bool,
    strike: bool,
    link: bool,
}

impl Flags {
    fn style(&self, s: MdStyle) -> Option<HighlightStyle> {
        if !self.bold && !self.italic && !self.strike && !self.link {
            return None;
        }
        Some(HighlightStyle {
            color: self.link.then_some(s.link),
            font_weight: self.bold.then_some(FontWeight::BOLD),
            font_style: self.italic.then_some(FontStyle::Italic),
            underline: self.link.then_some(UnderlineStyle {
                thickness: px(1.),
                color: Some(s.link),
                wavy: false,
            }),
            strikethrough: self.strike.then_some(StrikethroughStyle {
                thickness: px(1.),
                color: None,
            }),
            ..Default::default()
        })
    }
}

fn styled_text(text: String, highlights: Vec<(Range<usize>, HighlightStyle)>) -> StyledText {
    StyledText::new(SharedString::from(text)).with_highlights(highlights)
}

fn level_num(l: HeadingLevel) -> u8 {
    match l {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Render one markdown block to a gpui element.
pub fn render_markdown(md: &str, s: MdStyle) -> AnyElement {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, opts);

    let mut blocks: Vec<AnyElement> = Vec::new();

    // Inline accumulation for the current text-bearing block.
    let mut text = String::new();
    let mut hl: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    let mut flags = Flags::default();

    let mut heading: Option<u8> = None;
    let mut in_code = false;
    let mut code_text = String::new();
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut item_indent = 0usize;
    let mut item_marker: Option<String> = None;
    let mut quote_depth = 0usize;

    let push_styled = |text: &mut String, hl: &mut Vec<(Range<usize>, HighlightStyle)>, flags: &Flags, chunk: &str| {
        let start = text.len();
        text.push_str(chunk);
        if let Some(style) = flags.style(s) {
            hl.push((start..text.len(), style));
        }
    };

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => heading = Some(level_num(level)),
                Tag::Emphasis => flags.italic = true,
                Tag::Strong => flags.bold = true,
                Tag::Strikethrough => flags.strike = true,
                Tag::Link { .. } => flags.link = true,
                Tag::List(start) => list_stack.push(start),
                Tag::Item => {
                    let marker = match list_stack.last().copied().flatten() {
                        Some(n) => format!("{n}."),
                        None => "•".to_string(),
                    };
                    if let Some(Some(n)) = list_stack.last_mut() {
                        *n += 1;
                    }
                    item_indent = list_stack.len().saturating_sub(1);
                    item_marker = Some(marker);
                }
                Tag::BlockQuote(_) => quote_depth += 1,
                Tag::CodeBlock(_) => {
                    in_code = true;
                    code_text.clear();
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    blocks.push(flush_para(&mut text, &mut hl, s, quote_depth));
                }
                TagEnd::Heading(_) => {
                    let level = heading.take().unwrap_or(1);
                    blocks.push(flush_heading(&mut text, &mut hl, s, level));
                }
                TagEnd::Item => {
                    let marker = item_marker.take().unwrap_or_else(|| "•".to_string());
                    blocks.push(flush_item(&mut text, &mut hl, s, item_indent, marker));
                }
                TagEnd::Emphasis => flags.italic = false,
                TagEnd::Strong => flags.bold = false,
                TagEnd::Strikethrough => flags.strike = false,
                TagEnd::Link => flags.link = false,
                TagEnd::List(_) => {
                    list_stack.pop();
                }
                TagEnd::BlockQuote(_) => quote_depth = quote_depth.saturating_sub(1),
                TagEnd::CodeBlock => {
                    in_code = false;
                    blocks.push(code_block(&code_text, s));
                    code_text.clear();
                }
                _ => {}
            },
            Event::Text(t) => {
                if in_code {
                    code_text.push_str(&t);
                } else {
                    push_styled(&mut text, &mut hl, &flags, &t);
                }
            }
            Event::Code(t) => {
                let start = text.len();
                text.push_str(&t);
                hl.push((
                    start..text.len(),
                    HighlightStyle {
                        background_color: Some(s.code_bg),
                        color: Some(s.code_fg),
                        ..Default::default()
                    },
                ));
            }
            Event::SoftBreak => {
                if in_code {
                    code_text.push('\n');
                } else {
                    text.push(' ');
                }
            }
            Event::HardBreak => {
                if !in_code {
                    text.push('\n');
                }
            }
            Event::TaskListMarker(done) => {
                push_styled(&mut text, &mut hl, &flags, if done { "☑ " } else { "☐ " });
            }
            Event::Rule => {
                blocks.push(div().h(px(1.)).my_2().bg(s.border).into_any_element());
            }
            _ => {}
        }
    }

    // Any trailing inline text with no closing block tag.
    if !text.is_empty() {
        blocks.push(flush_para(&mut text, &mut hl, s, quote_depth));
    }

    div().flex().flex_col().gap_1().children(blocks).into_any_element()
}

fn flush_para(
    text: &mut String,
    hl: &mut Vec<(Range<usize>, HighlightStyle)>,
    s: MdStyle,
    quote_depth: usize,
) -> AnyElement {
    let content = styled_text(std::mem::take(text), std::mem::take(hl));
    if quote_depth > 0 {
        div()
            .border_l_2()
            .border_color(s.border)
            .pl_3()
            .py_1()
            .text_color(s.muted)
            .child(content)
            .into_any_element()
    } else {
        div().text_color(s.text).child(content).into_any_element()
    }
}

fn flush_heading(
    text: &mut String,
    hl: &mut Vec<(Range<usize>, HighlightStyle)>,
    s: MdStyle,
    level: u8,
) -> AnyElement {
    let (size, weight) = match level {
        1 => (px(26.), FontWeight::BOLD),
        2 => (px(21.), FontWeight::BOLD),
        3 => (px(17.), FontWeight::SEMIBOLD),
        _ => (px(15.), FontWeight::SEMIBOLD),
    };
    div()
        .mt_2()
        .text_size(size)
        .font_weight(weight)
        .text_color(s.text)
        .child(styled_text(std::mem::take(text), std::mem::take(hl)))
        .into_any_element()
}

fn flush_item(
    text: &mut String,
    hl: &mut Vec<(Range<usize>, HighlightStyle)>,
    s: MdStyle,
    indent: usize,
    marker: String,
) -> AnyElement {
    div()
        .flex()
        .flex_row()
        .gap_2()
        .pl(px(indent as f32 * 16.0))
        .text_color(s.text)
        .child(div().w(px(14.)).text_color(s.muted).child(SharedString::from(marker)))
        .child(
            div()
                .flex_1()
                .child(styled_text(std::mem::take(text), std::mem::take(hl))),
        )
        .into_any_element()
}

fn code_block(code: &str, s: MdStyle) -> AnyElement {
    let code = code.strip_suffix('\n').unwrap_or(code);
    div()
        .my_1()
        .p_3()
        .rounded(px(6.))
        .bg(s.code_bg)
        .text_color(s.code_fg)
        .text_size(px(13.))
        .font_family("monospace")
        .flex()
        .flex_col()
        .children(
            code.split('\n')
                .map(|line| div().child(SharedString::from(line.to_string())).into_any_element()),
        )
        .into_any_element()
}
