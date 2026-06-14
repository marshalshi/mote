use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{
        Block, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
    },
};
use std::sync::OnceLock;

use super::state::{App, AppState, SubagentView};

/// Paint a frame.
pub fn render(frame: &mut Frame, app: &mut App) {
    let full_area = frame.area();

    // Cap content width at 140 columns for readability on wide terminals
    let max_content_width: u16 = 140;
    let area = if full_area.width > max_content_width + 2 {
        let margin = (full_area.width - max_content_width) / 2;
        Rect::new(
            full_area.x + margin,
            full_area.y,
            max_content_width,
            full_area.height,
        )
    } else {
        full_area
    };

    let newline_count = app.input.matches('\n').count();
    let input_lines = {
        let base = (newline_count.saturating_add(1)).max(1).min(8) as u16 + 4; // +2 for top/bottom accent +2 for spacers
        // Permission prompt can be tall when args are large; reserve extra space.
        if app.pending_permission.is_some() {
            let confirm = app
                .pending_permission
                .as_ref()
                .map_or(false, |p| p.confirming_always);
            base.max(if confirm { 16 } else { 14 })
        } else {
            base
        }
    };
    let show_loading = app.loading_progress.is_some();
    let loading_height: u16 = if show_loading { 1 } else { 0 };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(input_lines),
            Constraint::Length(loading_height),
            Constraint::Length(2), // status bar + empty line below
        ])
        .split(area);

    render_response_area(frame, chunks[0], app);
    render_input_area(frame, chunks[1], app, app.input_accent);
    render_suggestions(frame, chunks[1], app, app.input_accent);
    if show_loading {
        render_loading_bar(frame, chunks[2], app);
    }
    render_status_bar(frame, chunks[3], app);

    if app.session_picker_open {
        render_session_picker(frame, full_area, app);
    }

    // Permission is handled inline inside render_input_area — no overlay needed
}

fn render_session_picker(frame: &mut Frame, area: Rect, app: &App) {
    let w = area.width.min(90).max(40);
    let h = area.height.min(20).max(8);
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    let rect = Rect::new(x, y, w, h);

    let block = Block::default().title(" Sessions ").borders(ratatui::widgets::Borders::ALL);
    frame.render_widget(block, rect);

    let inner = Rect::new(rect.x + 1, rect.y + 1, rect.width.saturating_sub(2), rect.height.saturating_sub(2));
    let mut lines: Vec<Line> = Vec::new();
    if app.session_picker_items.is_empty() {
        lines.push(Line::from("No sessions found."));
    } else {
        // Each session uses two lines, and we reserve two footer lines.
        let available_rows = inner.height.saturating_sub(2) as usize;
        let visible_items = (available_rows / 2).max(1);
        let total_items = app.session_picker_items.len();
        let (start, end) = session_picker_window(
            total_items,
            app.session_picker_index,
            visible_items,
        );

        for (i, s) in app.session_picker_items[start..end].iter().enumerate() {
            let i = start + i;
            let marker = if i == app.session_picker_index { ">" } else { " " };
            let name = s.summary.as_deref().unwrap_or("(no name)");
            lines.push(Line::from(vec![
                Span::styled(format!("{} {}", marker, name), Style::default().fg(if i == app.session_picker_index { Color::Yellow } else { Color::White })),
            ]));
            lines.push(Line::from(Span::styled(
                format!("   {}  {}  {} msgs", s.id, s.model, s.message_count),
                Style::default().fg(Color::DarkGray),
            )));
        }
        if end < total_items {
            lines.push(Line::from(Span::styled(
                format!("... {} more", total_items - end),
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "↑/↓ select • Enter load • Esc close",
        Style::default().fg(Color::DarkGray),
    )));
    frame.render_widget(Paragraph::new(Text::from(lines)), inner);
}

fn session_picker_window(
    total_items: usize,
    selected_index: usize,
    visible_items: usize,
) -> (usize, usize) {
    if total_items == 0 {
        return (0, 0);
    }
    let visible = visible_items.max(1).min(total_items);
    let selected = selected_index.min(total_items - 1);
    let mut start = selected.saturating_sub(visible / 2);
    if start + visible > total_items {
        start = total_items - visible;
    }
    (start, start + visible)
}

// ── Syntax highlighting ──────────────────────────────────

fn syntax_set() -> &'static syntect::parsing::SyntaxSet {
    static SET: OnceLock<syntect::parsing::SyntaxSet> = OnceLock::new();
    SET.get_or_init(syntect::parsing::SyntaxSet::load_defaults_newlines)
}
fn theme_set() -> &'static syntect::highlighting::ThemeSet {
    static TS: OnceLock<syntect::highlighting::ThemeSet> = OnceLock::new();
    TS.get_or_init(syntect::highlighting::ThemeSet::load_defaults)
}
fn code_theme() -> &'static syntect::highlighting::Theme {
    let ts = theme_set();
    ts.themes
        .get("base16-ocean.dark")
        .or_else(|| ts.themes.get("InspiredGitHub"))
        .or_else(|| ts.themes.values().next())
        .expect("syntect ships at least one built-in theme")
}
fn syntect_style_to_ratatui(s: syntect::highlighting::Style) -> Style {
    let fg = Color::Rgb(s.foreground.r, s.foreground.g, s.foreground.b);
    let mut style = Style::default().fg(fg);
    if s.font_style
        .contains(syntect::highlighting::FontStyle::BOLD)
    {
        style = style.add_modifier(Modifier::BOLD);
    }
    if s.font_style
        .contains(syntect::highlighting::FontStyle::ITALIC)
    {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if s.font_style
        .contains(syntect::highlighting::FontStyle::UNDERLINE)
    {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    style
}
fn highlight_code(lang: &str, code: &str) -> Vec<Line<'static>> {
    let ss = syntax_set();
    let syntax = ss
        .find_syntax_by_token(lang)
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    use syntect::easy::HighlightLines;
    let mut h = HighlightLines::new(syntax, code_theme());
    let mut lines = Vec::new();
    for line in code.lines() {
        let ranges = h.highlight_line(line, ss).unwrap_or_default();
        let spans: Vec<Span<'static>> = ranges
            .into_iter()
            .map(|(style, text)| {
                Span::styled(text.to_string(), syntect_style_to_ratatui(style))
            })
            .collect();
        lines.push(Line::from(spans));
    }
    lines
}

// ── Markdown rendering ──────────────────────────────────

/// Render markdown text into styled lines with accent bars.
/// `accent_prefix` is shown at the start of each line (e.g. `" ▌"` or `"  "`).
/// Handles: headers, bold, italic, code spans, lists, blockquotes, and fenced code blocks.
fn render_markdown(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    max_width: usize,
    accent_prefix: &str,
    accent_style: Style,
    _base_style: Style,
) {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

    let opts = Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(text, opts);

    // Style stack to handle nested formatting
    let mut bold = false;
    let mut italic = false;
    let mut in_heading = false;
    let mut in_blockquote = false;
    let mut list_depth: u32 = 0;
    let mut ordered_index: Option<u64> = None;
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();

    // Current line spans being built
    let mut current_spans: Vec<Span<'static>> = Vec::new();

    let flush_line = |lines: &mut Vec<Line<'static>>,
                      spans: &mut Vec<Span<'static>>,
                      accent: Style,
                      prefix: &str,
                      max_w: usize| {
        if spans.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(accent_prefix.to_string(), accent),
                Span::raw(prefix.to_string()),
            ]));
            return;
        }
        // Build the full text for word-wrapping measurement
        let full_text: String =
            spans.iter().map(|s| s.content.as_ref()).collect();
        let prefixed = format!("{}{}", prefix, full_text);
        let wrapped = word_wrap_line(&prefixed, max_w);
        for (i, part) in wrapped.iter().enumerate() {
            if i == 0 {
                let mut line_spans =
                    vec![Span::styled(accent_prefix.to_string(), accent)];
                // Re-apply the original styling to the first line
                line_spans.push(Span::styled(
                    part.clone(),
                    spans.first().map(|s| s.style).unwrap_or_default(),
                ));
                lines.push(Line::from(line_spans));
            } else {
                let mut line_spans =
                    vec![Span::styled(accent_prefix.to_string(), accent)];
                line_spans.push(Span::styled(
                    format!("{}{}", " ".repeat(prefix.len()), part),
                    spans.first().map(|s| s.style).unwrap_or_default(),
                ));
                lines.push(Line::from(line_spans));
            }
        }
        spans.clear();
    };

    for event in parser {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                in_heading = true;
                let _ = level; // all headings get bold treatment
            }
            Event::End(TagEnd::Heading(_)) => {
                // Flush heading line with bold
                let heading_text: String =
                    current_spans.iter().map(|s| s.content.as_ref()).collect();
                let wrapped = word_wrap_line(&heading_text, max_width);
                for part in wrapped {
                    lines.push(Line::from(vec![
                        Span::styled(accent_prefix.to_string(), accent_style),
                        Span::styled(
                            part,
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                    ]));
                }
                current_spans.clear();
                in_heading = false;
            }
            Event::Start(Tag::Strong) => bold = true,
            Event::End(TagEnd::Strong) => bold = false,
            Event::Start(Tag::Emphasis) => italic = true,
            Event::End(TagEnd::Emphasis) => italic = false,
            Event::Start(Tag::BlockQuote(_)) => in_blockquote = true,
            Event::End(TagEnd::BlockQuote(_)) => in_blockquote = false,
            Event::Start(Tag::List(start)) => {
                list_depth += 1;
                ordered_index = start;
            }
            Event::End(TagEnd::List(_)) => {
                list_depth = list_depth.saturating_sub(1);
                if list_depth == 0 {
                    ordered_index = None;
                }
            }
            Event::Start(Tag::Item) => {}
            Event::End(TagEnd::Item) => {
                // Flush the list item
                let item_text: String =
                    current_spans.iter().map(|s| s.content.as_ref()).collect();
                let indent = "  ".repeat(list_depth.saturating_sub(1) as usize);
                let bullet = if let Some(ref mut idx) = ordered_index {
                    let s = format!("{}{}. ", indent, idx);
                    *idx += 1;
                    s
                } else {
                    format!(
                        "{} {} ",
                        indent,
                        if list_depth <= 1 {
                            "\u{2022}"
                        } else {
                            "\u{25E6}"
                        }
                    )
                };
                let prefixed = format!("{}{}", bullet, item_text);
                let style =
                    current_spans.first().map(|s| s.style).unwrap_or_default();
                let wrapped = word_wrap_line(&prefixed, max_width);
                for (i, part) in wrapped.iter().enumerate() {
                    let mut line_spans = vec![Span::styled(
                        accent_prefix.to_string(),
                        accent_style,
                    )];
                    if i > 0 {
                        line_spans.push(Span::styled(
                            format!("{}{}", " ".repeat(bullet.len()), part),
                            style,
                        ));
                    } else {
                        line_spans.push(Span::styled(part.clone(), style));
                    }
                    lines.push(Line::from(line_spans));
                }
                current_spans.clear();
            }
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code_block = true;
                code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                code_buf.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                // Syntax-highlight the code block
                let code = code_buf.trim_matches('\n');
                if !code.is_empty() {
                    let highlighted = highlight_code(&code_lang, code);
                    for hl_line in highlighted {
                        let mut spans = vec![Span::styled(
                            accent_prefix.to_string(),
                            accent_style,
                        )];
                        spans.extend(hl_line.into_iter());
                        lines.push(Line::from(spans));
                    }
                }
                in_code_block = false;
                code_buf.clear();
            }
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                if !current_spans.is_empty() {
                    let prefix = if in_blockquote { "  │ " } else { "" };
                    flush_line(
                        lines,
                        &mut current_spans,
                        accent_style,
                        prefix,
                        max_width,
                    );
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                // Just show the link text in blue underlined
                let _ = dest_url;
            }
            Event::End(TagEnd::Link) => {}
            Event::Text(text) => {
                if in_code_block {
                    code_buf.push_str(&text);
                } else if in_heading {
                    current_spans.push(Span::styled(
                        text.to_string(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ));
                } else {
                    let mut style = Style::default();
                    if bold {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if italic {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    if in_blockquote {
                        style = style.fg(Color::DarkGray);
                    }
                    current_spans.push(Span::styled(text.to_string(), style));
                }
            }
            Event::Code(code) => {
                // Inline code: dim background style
                current_spans.push(Span::styled(
                    format!("`{}`", code),
                    Style::default().fg(Color::Yellow),
                ));
            }
            Event::SoftBreak | Event::HardBreak => {
                if !current_spans.is_empty() {
                    let prefix = if in_blockquote { "  │ " } else { "" };
                    flush_line(
                        lines,
                        &mut current_spans,
                        accent_style,
                        prefix,
                        max_width,
                    );
                }
            }
            Event::Rule => {
                let rule = "─".repeat(max_width.min(40));
                lines.push(Line::from(vec![
                    Span::styled(accent_prefix.to_string(), accent_style),
                    Span::styled(rule, Style::default().fg(Color::DarkGray)),
                ]));
            }
            _ => {}
        }
    }

    // Flush any remaining spans
    if !current_spans.is_empty() {
        let prefix = if in_blockquote { "  │ " } else { "" };
        flush_line(lines, &mut current_spans, accent_style, prefix, max_width);
    }
}

// ── Build response lines with accent bars ───────────────

/// Word-wrap a single line to fit within `max_width` display columns.
/// Splits on `\n` first, then wraps each segment independently.
/// Uses `unicode-width` for proper display width measurement.
fn word_wrap_line(line: &str, max_width: usize) -> Vec<String> {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let mut result = Vec::new();

    for segment in line.split('\n') {
        if segment.is_empty() {
            result.push(String::new());
            continue;
        }
        let mut remaining = segment;
        while !remaining.is_empty() {
            let w = UnicodeWidthStr::width(remaining);
            if w <= max_width {
                result.push(remaining.to_string());
                break;
            }
            // Find character boundary where display width exceeds max_width
            let mut break_at = remaining.len();
            let mut cur_w = 0;
            for (i, c) in remaining.char_indices() {
                let cw = UnicodeWidthChar::width(c).unwrap_or(0);
                if cur_w + cw > max_width {
                    break_at = i;
                    break;
                }
                cur_w += cw;
            }
            // Backtrack to the last space for clean word-wrapping
            let space_break = remaining[..break_at].rfind(' ');
            if let Some(sp) = space_break {
                result.push(remaining[..sp].to_string());
                remaining = remaining[sp + 1..].trim_start();
            } else if break_at > 0 {
                // No space — hard break (e.g. long URL)
                result.push(remaining[..break_at].to_string());
                remaining = remaining[break_at..].trim_start();
            } else {
                // break_at == 0 means the first character already exceeds max_width.
                // Force-break at the first character boundary to always make progress.
                let first_char_count = {
                    let c = remaining.chars().next().unwrap_or(' ');
                    c.len_utf8()
                };
                // Push the first character (even if it exceeds max_width)
                result.push(remaining[..first_char_count].to_string());
                remaining = remaining[first_char_count..].trim_start();
                // If we still haven't advanced, force-skip one byte to break the loop
                if remaining.is_empty() && first_char_count == 0 {
                    break;
                }
            }
        }
    }

    result
}

/// Push one or more lines for `content`, wrapping to `max_width`.
/// Each line gets `accent_prefix + accent_style` prefix and `content_style` for the text.
fn push_accent_lines(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    max_width: usize,
    accent_prefix: &str,
    accent_style: Style,
    content_style: Style,
) {
    for segment in content.split('\n') {
        let wrapped = word_wrap_line(segment, max_width);
        for part in wrapped {
            lines.push(Line::from(vec![
                Span::styled(accent_prefix.to_string(), accent_style),
                Span::styled(part, content_style.clone()),
            ]));
        }
    }
}

fn push_tool_changes(
    lines: &mut Vec<Line<'static>>,
    changes: &[marshaling_protocol::FileChange],
) {
    for ch in changes {
        match ch.kind {
            marshaling_protocol::FileChangeKind::Modified => {
                lines.push(Line::from(vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(
                        format!(" diff -- {}", ch.path),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
                for dl in &ch.diff_lines {
                    let (prefix, color) = match dl.kind {
                        marshaling_protocol::DiffLineKind::Added => {
                            ("+", Color::Green)
                        }
                        marshaling_protocol::DiffLineKind::Removed => {
                            ("-", Color::Red)
                        }
                        marshaling_protocol::DiffLineKind::Context => {
                            (" ", Color::DarkGray)
                        }
                    };
                    lines.push(Line::from(vec![
                        Span::styled("    ", Style::default()),
                        Span::styled(
                            format!(" {}{}", prefix, dl.content),
                            Style::default().fg(color),
                        ),
                    ]));
                }
                if ch.truncated {
                    lines.push(Line::from(vec![
                        Span::styled("    ", Style::default()),
                        Span::styled(
                            " [diff truncated]",
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }
            }
            marshaling_protocol::FileChangeKind::Added => {
                lines.push(Line::from(vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(
                        format!(" ! new file added: {}", ch.path),
                        Style::default().fg(Color::Yellow),
                    ),
                ]));
            }
            marshaling_protocol::FileChangeKind::Removed => {
                lines.push(Line::from(vec![
                    Span::styled("    ", Style::default()),
                    Span::styled(
                        format!(" ! file removed: {}", ch.path),
                        Style::default().fg(Color::Yellow),
                    ),
                ]));
            }
        }
    }
}

fn build_lines(app: &App, content_width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let grey_content = Style::default().fg(Color::DarkGray);

    // Welcome screen when no messages and not streaming
    if app.messages.is_empty()
        && app.stream_buffer.is_empty()
        && app.tool_calls.is_empty()
    {
        lines.push(Line::from(""));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Welcome to Mote",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  Type a message to get started.",
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(Span::styled(
            "  /help for commands, Ctrl+C to quit.",
            Style::default().fg(Color::DarkGray),
        )));
        return lines;
    }

    let mut prev_role: Option<&crate::llm::Role> = None;

    for msg in &app.messages {
        // Determine accent prefix and style based on role
        // User messages get a colored accent bar; assistant messages get blank indentation.
        let (accent_prefix, accent_style) = match msg.role {
            crate::llm::Role::User => {
                (" ▌  ", Style::default().fg(app.user_accent))
            }
            crate::llm::Role::Assistant => ("    ", Style::default()),
        };

        // Override for error messages — always use colored accent bar
        let content_style = if msg.source == super::state::MessageSource::Error
        {
            Style::default().fg(Color::Red)
        } else {
            Style::default()
        };
        let accent_prefix = if msg.source == super::state::MessageSource::Error
        {
            " ▌  "
        } else {
            accent_prefix
        };
        let accent_style = if msg.source == super::state::MessageSource::Error {
            Style::default().fg(Color::Red)
        } else {
            accent_style
        };

        // Separator between role groups (when role changes)
        let role_changed = prev_role.map_or(true, |r| *r != msg.role);
        if role_changed {
            if prev_role.is_some() {
                lines.push(Line::from(""));
            }
        }
        prev_role = Some(&msg.role);

        // Render thinking content with blank side bar, if present
        if let Some(ref thinking) = msg.thinking {
            if !thinking.is_empty() {
                push_accent_lines(
                    &mut lines,
                    thinking,
                    content_width,
                    "    ",
                    Style::default(),
                    grey_content,
                );
            }
        }

        // Empty line between thinking and output if both are present
        if msg.thinking.as_ref().map_or(false, |t| !t.is_empty())
            && !msg.content.is_empty()
        {
            lines.push(Line::from(Span::styled(
                accent_prefix.to_string(),
                accent_style,
            )));
        }

        // Render the regular message content with markdown formatting
        if !msg.content.is_empty() {
            // Render with markdown parser (each line already has accent prefix)
            render_markdown(
                &mut lines,
                &msg.content,
                content_width,
                accent_prefix,
                accent_style,
                content_style,
            );
        }
    }

    // Streaming reasoning content (grey styling) — shown during live agent streaming
    if !app.reasoning_buffer.is_empty() {
        push_accent_lines(
            &mut lines,
            &app.reasoning_buffer,
            content_width,
            "    ",
            Style::default(),
            grey_content,
        );
    }

    // Streaming content (assistant — blank side bar)
    if !app.stream_buffer.is_empty() {
        push_accent_lines(
            &mut lines,
            &app.stream_buffer,
            content_width,
            "    ",
            Style::default(),
            Style::default(),
        );
        lines.push(Line::from(""));
    }

    // Tool calls (assistant — blank side bar) with animated spinner and args preview
    let spinner_frame = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        / 200)
        % 4;
    let spinner_char = match spinner_frame {
        0 => "◐",
        1 => "◓",
        2 => "◑",
        _ => "◒",
    };

    for tc in &app.tool_calls {
        let (symbol, color) = match &tc.status {
            marshaling_protocol::ToolStatus::Running => {
                (spinner_char, Color::Yellow)
            }
            marshaling_protocol::ToolStatus::Success => ("✓", Color::Green),
            marshaling_protocol::ToolStatus::Failed(_) => ("✗", Color::Red),
        };
        let text = format!(" {} {}", symbol, tc.name);
        let mut spans = vec![Span::styled("    ", Style::default())];
        spans.push(Span::styled(text, Style::default().fg(color)));
        lines.push(Line::from(spans));
        if !tc.changes.is_empty() {
            push_tool_changes(&mut lines, &tc.changes);
        }
    }
    if !app.tool_calls.is_empty() {
        lines.push(Line::from(""));
    }

    // Empty line after output accent for spacing before input area
    lines.push(Line::from(""));

    lines
}

// ── Response area ───────────────────────────────────────

fn render_response_area(frame: &mut Frame, area: Rect, app: &App) {
    let content_width = area.width.saturating_sub(7) as usize; // 4 for accent bar + 2 right padding + 1 scrollbar
    let lines = if let Some(idx) = app.current_subagent_index {
        if let Some(sv) = app.subagent_views.get(idx) {
            build_subagent_lines(
                sv,
                content_width,
                idx,
                app.subagent_views.len(),
            )
        } else {
            build_lines(app, content_width)
        }
    } else {
        build_lines(app, content_width)
    };
    let available_height = area.height.saturating_sub(1) as usize;
    let total_lines = lines.len();
    let max_scroll = if total_lines > available_height {
        total_lines - available_height
    } else {
        0
    };
    let scroll = if app.auto_scroll {
        max_scroll
    } else {
        max_scroll.saturating_sub(app.scroll_offset)
    };
    let visible: Vec<Line> = if total_lines > scroll {
        lines[scroll..].to_vec()
    } else {
        lines
    };
    let text = Text::from(visible);
    let paragraph = Paragraph::new(text); // no wrap — we already pre-wrapped
    frame.render_widget(paragraph, area);

    // Scrollbar (only when content exceeds visible area)
    if total_lines > available_height {
        let mut scrollbar_state = ScrollbarState::new(total_lines)
            .position(scroll)
            .viewport_content_length(available_height);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .style(Style::default().fg(Color::DarkGray));
        frame.render_stateful_widget(scrollbar, area, &mut scrollbar_state);
    }
}

fn build_subagent_lines(
    sv: &SubagentView,
    content_width: usize,
    idx: usize,
    total: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let grey_content = Style::default().fg(Color::DarkGray);

    // Header showing which subagent (with index and total)
    let status = if sv.done { "done" } else { "running" };
    lines.push(Line::from(Span::styled(
        format!(
            "     Sub-agent: {} ({}/{}) — {}",
            sv.name,
            idx + 1,
            total,
            status
        ),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled("    ", Style::default())));

    // Reasoning content (blank side bar)
    if !sv.reasoning_buffer.is_empty() {
        push_accent_lines(
            &mut lines,
            &sv.reasoning_buffer,
            content_width,
            "    ",
            Style::default(),
            grey_content,
        );
    }

    // Stream buffer (in-progress text) — blank side bar for assistant
    if !sv.stream_buffer.is_empty() {
        push_accent_lines(
            &mut lines,
            &sv.stream_buffer,
            content_width,
            "    ",
            Style::default(),
            Style::default(),
        );
    }

    // Tool calls (with animated spinner) — blank side bar
    let spinner_frame = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        / 200)
        % 4;
    let spinner_char = match spinner_frame {
        0 => "◐",
        1 => "◓",
        2 => "◑",
        _ => "◒",
    };

    for tc in &sv.tool_calls {
        let (symbol, color) = match &tc.status {
            marshaling_protocol::ToolStatus::Running => {
                (spinner_char, Color::Yellow)
            }
            marshaling_protocol::ToolStatus::Success => ("✓", Color::Green),
            marshaling_protocol::ToolStatus::Failed(_) => ("✗", Color::Red),
        };
        let text = format!(" {} {}", symbol, tc.name);
        let mut spans = vec![Span::styled("    ", Style::default())];
        spans.push(Span::styled(text, Style::default().fg(color)));
        lines.push(Line::from(spans));
        if !tc.changes.is_empty() {
            push_tool_changes(&mut lines, &tc.changes);
        }
    }

    // Final content (when done)
    if sv.done && !sv.content.is_empty() {
        push_accent_lines(
            &mut lines,
            &sv.content,
            content_width,
            "    ",
            Style::default(),
            grey_content,
        );
    }

    lines
}

// ── Input area (no border, accent bar) ───────────────────

/// Convert a JSON args string into YAML-like display lines.
/// Handles flat objects, simple values, and arrays of strings.
fn json_to_yaml_lines(json_str: &str, max_width: usize) -> Vec<String> {
    if json_str.is_empty() || json_str == "null" {
        return Vec::new();
    }
    // Try to parse as JSON Value
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(json_str) {
        match val {
            serde_json::Value::Object(map) => {
                let mut lines = Vec::new();
                for (key, value) in map {
                    let val_str = match &value {
                        serde_json::Value::String(s) => s.clone(),
                        serde_json::Value::Number(n) => n.to_string(),
                        serde_json::Value::Bool(b) => b.to_string(),
                        serde_json::Value::Null => "null".into(),
                        // For nested objects/arrays, use compact JSON
                        other => other.to_string(),
                    };
                    // Truncate long values
                    let display = if val_str.len() > max_width.saturating_sub(4)
                    {
                        let end = val_str
                            .char_indices()
                            .take(max_width.saturating_sub(7))
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(val_str.len());
                        format!("{}...", &val_str[..end])
                    } else {
                        val_str
                    };
                    lines.push(format!("{}: {}", key, display));
                }
                lines
            }
            serde_json::Value::String(s) => {
                vec![if s.len() > max_width {
                    let end = s
                        .char_indices()
                        .take(max_width.saturating_sub(3))
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(s.len());
                    format!("{}...", &s[..end])
                } else {
                    s
                }]
            }
            serde_json::Value::Array(arr) => arr
                .iter()
                .map(|v| match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                })
                .collect(),
            other => vec![other.to_string()],
        }
    } else {
        // Not valid JSON — show raw string (maybe already YAML-like)
        vec![json_str.to_string()]
    }
}

fn render_input_area(frame: &mut Frame, area: Rect, app: &App, accent: Color) {
    let input_disabled = matches!(
        app.state,
        AppState::WaitingResponse | AppState::AgentRunning
    );
    let input_style = if input_disabled {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    };
    let a_disabled = if input_disabled {
        Color::DarkGray
    } else {
        accent
    };
    let a_style = Style::default().fg(a_disabled);
    // 4 for accent bar + 2 for right padding
    let content_width = area.width.saturating_sub(6) as usize;

    let mut lines: Vec<Line> = Vec::new();

    // Empty spacer line above the accent padding
    lines.push(Line::from(Span::styled("    ", a_style)));

    // Top accent padding
    lines.push(Line::from(Span::styled(" ▌  ", a_style)));

    if let Some(ref perm) = app.pending_permission {
        // ── Permission prompt replaces input box ──────────
        let mut args_lines = json_to_yaml_lines(&perm.args, content_width);
        let base_lines_no_args: u16 =
            if perm.confirming_always { 9 } else { 7 };
        let max_args_lines = area
            .height
            .saturating_sub(base_lines_no_args)
            .saturating_sub(1) as usize;
        if args_lines.len() > max_args_lines {
            if max_args_lines == 0 {
                args_lines.clear();
            } else if max_args_lines == 1 {
                args_lines = vec!["... (args truncated)".into()];
            } else {
                args_lines.truncate(max_args_lines - 1);
                args_lines.push("... (args truncated)".into());
            }
        }
        lines.push(Line::from(vec![
            Span::styled(" ▌  ", a_style),
            Span::styled(
                " 🔒 Permission Required ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" ▌  ", a_style),
            Span::styled(format!("tool: {}", perm.tool_name), input_style),
        ]));
        if !args_lines.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(" ▌  ", a_style),
                Span::styled("args:", Style::default().fg(Color::DarkGray)),
            ]));
            for arg_line in &args_lines {
                lines.push(Line::from(vec![
                    Span::styled(" ▌  ", a_style),
                    Span::styled(
                        format!("  {}", arg_line),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
        }
        if perm.confirming_always {
            // ── Confirmation popup ──────────
            lines.push(Line::from(vec![
                Span::styled(" ▌  ", a_style),
                Span::styled(
                    " ⚠️ Confirm Allow Always ",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled(" ▌  ", a_style),
                Span::styled(
                    format!(
                        "  Auto-allow all future `{}` calls?",
                        perm.tool_name
                    ),
                    input_style,
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled(" ▌  ", a_style),
                Span::styled(
                    "  [Y] Confirm  [N] Cancel",
                    Style::default().fg(accent),
                ),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled(" ▌  ", a_style),
                Span::styled(
                    " [Y] Allow Once  [A] Allow Always  [N] Deny",
                    Style::default().fg(accent),
                ),
            ]));
        }
        lines.push(Line::from(Span::styled(" ▌  ", a_style)));
    } else {
        // Input content lines — word-wrapped with accent bar and prompt symbol
        let wrapped =
            word_wrap_line(&app.input, content_width.saturating_sub(2)); // -2 for prompt "❯ "
        for (i, text_line) in wrapped.iter().enumerate() {
            let prompt = if i == 0 { "❯ " } else { "  " };
            lines.push(Line::from(vec![
                Span::styled(" ▌  ", a_style),
                Span::styled(prompt.to_string(), a_style),
                Span::styled(text_line.clone(), input_style),
            ]));
        }
        // Bottom accent padding
        lines.push(Line::from(Span::styled(" ▌  ", a_style)));
    }

    // Empty spacer line below the accent padding
    lines.push(Line::from(Span::styled("    ", a_style)));

    let text = Text::from(lines);
    let paragraph = Paragraph::new(text);
    frame.render_widget(paragraph, area);

    // Cursor — only when idle and no pending permission
    if app.pending_permission.is_none() && app.state == AppState::Idle {
        let prompt_width = 2; // "❯ "
        let (col, visual_row) = cursor_pos_after_wrap(
            &app.input,
            app.input_cursor,
            content_width.saturating_sub(prompt_width),
        );
        let content_x = 4 + prompt_width + col; // 4 for accent bar " ▌  " + prompt
        let cx = area.x
            + content_x.min(area.width.saturating_sub(6) as usize) as u16;
        let cy = area.y + 2 + visual_row as u16; // +1 for spacer +1 for top accent padding
        frame.set_cursor_position(ratatui::prelude::Position::new(cx, cy));
    }
}

/// Compute cursor (col, visual_row) after word-wrapping the text up to `byte_offset`.
/// `visual_row` counts wrapped lines (0 = first visual line).
fn cursor_pos_after_wrap(
    text: &str,
    byte_offset: usize,
    max_width: usize,
) -> (usize, usize) {
    use unicode_width::UnicodeWidthStr;
    let text_before = if byte_offset <= text.len() {
        &text[..byte_offset]
    } else {
        text
    };
    let wrapped = word_wrap_line(text_before, max_width);
    let visual_row = wrapped.len().saturating_sub(1);
    let col = if let Some(last) = wrapped.last() {
        UnicodeWidthStr::width(last.as_str())
    } else {
        0
    };
    (col, visual_row)
}

// ── Status bar ──────────────────────────────────────────

fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    // Agent name on the left, follows current accent color. Blinks when agent is running.
    let is_running = matches!(
        app.state,
        AppState::AgentRunning | AppState::WaitingResponse
    );
    let mut agent_style = Style::default().fg(app.input_accent);
    if is_running {
        agent_style = agent_style.add_modifier(Modifier::SLOW_BLINK);
    }
    // Capitalize agent name for display
    let display_name = {
        let s = app.current_agent.as_str();
        let mut chars = s.chars();
        match chars.next() {
            None => String::new(),
            Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
        }
    };
    let server_status = match &app.server_health {
        crate::tui::state::ServerHealth::Connected
        | crate::tui::state::ServerHealth::Unknown => "".into(),
        crate::tui::state::ServerHealth::Disconnected(reason) => {
            format!(" (disconnected: {reason})")
        }
    };
    let left = format!(" {}{} ", display_name, server_status);

    // Right side: model, subagent info, tokens (no agent name — it's on the left now)
    let right_info = if let Some(idx) = app.current_subagent_index {
        if let Some(sv) = app.subagent_views.get(idx) {
            let status = if sv.done { "done" } else { "running" };
            let skill = app
                .current_skill
                .as_ref()
                .map(|s| format!(" | skill:{}", s))
                .unwrap_or_default();
            format!(
                " {} | Sub: {} ({}/{}) {}{} | in:{} out:{} ",
                app.model_info,
                sv.name,
                idx + 1,
                app.subagent_views.len(),
                status,
                skill,
                app.tokens_input,
                app.tokens_output
            )
        } else {
            format!(
                " {} | in:{} out:{} ",
                app.model_info, app.tokens_input, app.tokens_output
            )
        }
    } else {
        let skill = app
            .current_skill
            .as_ref()
            .map(|s| format!(" | skill:{}", s))
            .unwrap_or_default();
        format!(
            " {}{} | in:{} out:{} ",
            app.model_info, skill, app.tokens_input, app.tokens_output
        )
    };

    let style = Style::default().fg(Color::DarkGray);
    let total = area.width as usize;
    let hints = if matches!(
        app.state,
        AppState::AgentRunning | AppState::WaitingResponse
    ) {
        " Esc, Esc: stop agent | Ctrl+C: quit | /help: commands "
    } else {
        " Ctrl+C: quit | /help: commands "
    };
    let right = if total > left.len() + right_info.len() {
        right_info
    } else {
        String::new()
    };
    let used = left.len() + right.len();
    let middle = if total > used + hints.len() {
        hints.to_string()
    } else {
        String::new()
    };
    let padding = total.saturating_sub(left.len() + middle.len() + right.len());
    let pad_left = padding / 2;
    let pad_right = padding - pad_left;

    let mut spans = vec![Span::styled(left, agent_style)];
    if !middle.is_empty() {
        spans.push(Span::styled(" ".repeat(pad_left), style));
        spans.push(Span::styled(middle, Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(" ".repeat(pad_right), style));
    }
    if !right.is_empty() {
        spans.push(Span::styled(right, style));
    }
    frame.render_widget(Paragraph::new(Text::from(Line::from(spans))), area);
}

/// Loading bar: shows a spinner with context during agent activity.
fn render_loading_bar(frame: &mut Frame, area: Rect, app: &App) {
    let spinner = match (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        / 200)
        % 4
    {
        0 => "◐",
        1 => "◓",
        2 => "◑",
        3 => "◒",
        _ => "◐",
    };

    // Show context: if a tool is running, show its name
    let context = app
        .tool_calls
        .iter()
        .find(|tc| {
            matches!(tc.status, marshaling_protocol::ToolStatus::Running)
        })
        .map(|tc| format!("Running: {}", tc.name))
        .unwrap_or_else(|| "Thinking...".into());

    let style = Style::default().fg(Color::Cyan);
    frame.render_widget(
        Paragraph::new(Text::from(Line::from(Span::styled(
            format!(" {} {} ", spinner, context),
            style,
        )))),
        area,
    );
}

// ── Suggestions popup ───────────────────────────────────

fn render_suggestions(
    frame: &mut Frame,
    input_area: Rect,
    app: &App,
    accent: Color,
) {
    if app.suggestions.is_empty()
        || app.state != AppState::Idle
        || app.pending_permission.is_some()
    {
        return;
    }
    let count = app.suggestions.len().min(10) as u16;
    let popup_height = count + 2;
    let popup_y = input_area.y.saturating_sub(popup_height);
    if popup_y == 0 && input_area.y > popup_height + 1 {
        return;
    }

    let popup_area = Rect::new(
        input_area.x,
        popup_y,
        input_area.width.min(60),
        popup_height,
    );
    frame.render_widget(ratatui::widgets::Clear, popup_area);

    let mut lines: Vec<Line> = Vec::new();
    for (i, sug) in app.suggestions.iter().enumerate().take(10) {
        let selected = i == app.suggestion_index.saturating_sub(1);
        let style = if selected {
            Style::default().fg(Color::Black).bg(accent)
        } else {
            Style::default().fg(Color::White)
        };
        lines.push(Line::from(Span::styled(sug.clone(), style)));
    }
    let block = Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_style(Style::default().fg(accent));
    frame.render_widget(
        Paragraph::new(Text::from(lines)).block(block),
        popup_area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_word_wrap_short_line_no_wrap() {
        let result = word_wrap_line("hello", 80);
        assert_eq!(result, vec!["hello"]);
    }

    #[test]
    fn test_word_wrap_empty_string() {
        let result = word_wrap_line("", 80);
        assert_eq!(result, vec![""]);
    }

    #[test]
    fn test_word_wrap_newline_only() {
        let result = word_wrap_line("\n", 80);
        assert_eq!(result, vec!["", ""]);
    }

    #[test]
    fn test_word_wrap_newline_split() {
        let result = word_wrap_line("hello\nworld", 80);
        assert_eq!(result, vec!["hello", "world"]);
    }

    #[test]
    fn test_word_wrap_exact_width() {
        let line = "a".repeat(10);
        let result = word_wrap_line(&line, 10);
        assert_eq!(result, vec![line]);
    }

    #[test]
    fn test_word_wrap_exceeds_width() {
        let result = word_wrap_line("hello world foo bar", 10);
        assert!(result.len() >= 2);
        for segment in &result {
            assert!(segment.len() <= 10, "segment '{}' exceeds width", segment);
        }
    }

    #[test]
    fn test_word_wrap_narrow_terminal_no_infinite_loop() {
        let result = word_wrap_line("hello", 0);
        assert_eq!(result.len(), 5, "each char should be separate at width 0");
    }

    #[test]
    fn test_word_wrap_narrow_terminal_width_1() {
        let result = word_wrap_line("hi", 1);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_cursor_pos_after_wrap_simple() {
        let (col, row) = cursor_pos_after_wrap("hello", 5, 80);
        assert_eq!(col, 5);
        assert_eq!(row, 0);
    }

    #[test]
    fn test_cursor_pos_after_wrap_newline() {
        let (col, row) = cursor_pos_after_wrap("hello\nwor", 9, 80);
        assert_eq!(col, 3);
        assert_eq!(row, 1);
    }

    #[test]
    fn test_cursor_pos_after_wrap_empty() {
        let (col, row) = cursor_pos_after_wrap("", 0, 80);
        assert_eq!(col, 0);
        assert_eq!(row, 0);
    }

    #[test]
    fn test_session_picker_window_centers_and_clamps() {
        assert_eq!(session_picker_window(0, 0, 4), (0, 0));
        assert_eq!(session_picker_window(10, 0, 3), (0, 3));
        assert_eq!(session_picker_window(10, 5, 3), (4, 7));
        assert_eq!(session_picker_window(10, 9, 3), (7, 10));
        assert_eq!(session_picker_window(4, 100, 3), (1, 4));
    }

    // ── Markdown rendering tests ───────────────────────────

    /// Helper: render markdown and return the raw text content (without accent bars)
    fn md_text(input: &str) -> Vec<String> {
        let mut lines = Vec::new();
        let accent = Style::default().fg(Color::Cyan);
        let base = Style::default();
        render_markdown(&mut lines, input, 80, " ▌", accent, base);
        lines
            .iter()
            .map(|line| {
                // Skip the accent bar span and collect text
                line.spans
                    .iter()
                    .skip(1) // skip " ▌"
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn test_markdown_plain_text() {
        let result = md_text("Hello world");
        assert!(result.iter().any(|l| l.contains("Hello world")));
    }

    #[test]
    fn test_markdown_heading() {
        let result = md_text("# My Heading");
        assert!(result.iter().any(|l| l.contains("My Heading")));
    }

    #[test]
    fn test_markdown_bold_and_italic() {
        let result = md_text("**bold** and *italic*");
        let joined: String = result.join(" ");
        assert!(joined.contains("bold"));
        assert!(joined.contains("italic"));
    }

    #[test]
    fn test_markdown_inline_code() {
        let result = md_text("Use `foo()` here");
        let joined: String = result.join(" ");
        assert!(joined.contains("`foo()`"));
    }

    #[test]
    fn test_markdown_code_block() {
        let result = md_text("```rust\nfn main() {}\n```");
        let joined: String = result.join("\n");
        assert!(joined.contains("fn main()"));
    }

    #[test]
    fn test_markdown_unordered_list() {
        let result = md_text("- item one\n- item two");
        let joined: String = result.join("\n");
        assert!(joined.contains("item one"));
        assert!(joined.contains("item two"));
    }

    #[test]
    fn test_markdown_ordered_list() {
        let result = md_text("1. first\n2. second");
        let joined: String = result.join("\n");
        assert!(joined.contains("first"));
        assert!(joined.contains("second"));
    }

    #[test]
    fn test_markdown_blockquote() {
        let result = md_text("> quoted text");
        let joined: String = result.join(" ");
        assert!(joined.contains("quoted text"));
    }

    #[test]
    fn test_markdown_horizontal_rule() {
        let result = md_text("above\n\n---\n\nbelow");
        let joined: String = result.join("\n");
        assert!(joined.contains("─")); // HR rendered as ─ characters
        assert!(joined.contains("above"));
        assert!(joined.contains("below"));
    }

    #[test]
    fn test_markdown_empty_input() {
        let result = md_text("");
        // Should produce no lines (or only empty accent lines)
        assert!(result.iter().all(|l| l.trim().is_empty()));
    }

    #[test]
    fn test_markdown_mixed_content() {
        let input = "# Title\n\nSome **bold** text.\n\n```\ncode here\n```\n\n- list item";
        let result = md_text(input);
        let joined: String = result.join("\n");
        assert!(joined.contains("Title"));
        assert!(joined.contains("bold"));
        assert!(joined.contains("code here"));
        assert!(joined.contains("list item"));
    }
}
