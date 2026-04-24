/// Render — matches rustyclaw's visual style exactly.
///
/// Welcome screen:
///   ─ rustyclaw v0.1.0 ──────────────────────────────────────────
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

// Orange/amber — rustyclaw accent (dark theme default)
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

    // Collect input to String once — reused in both height calc and draw_input.
    let full_input: String = app.input.iter().collect();
    let usable_w = area.width.saturating_sub(2) as usize; // ">" + space prefix
    let visual_lines: u16 = full_input
        .split('\n')
        .map(|line| {
            let n = line.chars().count();
            n.div_ceil(usable_w).max(1) as u16
        })
        .sum();
    let input_height = visual_lines.clamp(1, 8);

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
    draw_input(f, outer[2], app, &full_input, tc);
    draw_status(f, outer[3], app, tc);

    if app.overlay.is_some() {
        draw_overlay(f, area, app, tc);
    } else if app.browse_approval.is_some() {
        draw_browse_approval(f, area, app, tc);
    } else if app.pending_permission.is_some() {
        draw_permission(f, area, app, tc);
    } else if app.pending_user_question.is_some() {
        draw_ask_user(f, area, app);
    }
}

// ── Welcome banner — 2-column bordered box matching the TS rustyclaw fork ──────

fn draw_banner(f: &mut Frame, area: Rect, app: &App, tc: ThemeColors) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(tc.accent))
        .title(Span::styled(
            format!(" rustyclaw v{VERSION} "),
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
    let cwd_display = if app.cached_cwd.len() > max_cwd {
        format!(
            "…{}",
            &app.cached_cwd[app.cached_cwd.len().saturating_sub(max_cwd - 1)..]
        )
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
                // Full-width dimmed row with dark background — matches rustyclaw
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

            EntryKind::Error => {
                lines.push(Line::from(vec![
                    Span::styled(
                        "✖ ",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(entry.text.to_owned(), Style::default().fg(Color::Red)),
                ]));
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

// ── Input line (no border — matches rustyclaw's plain "> " prompt) ────────────

fn draw_input(f: &mut Frame, area: Rect, app: &App, full_input: &str, tc: ThemeColors) {
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

    // full_input already computed in draw() — reuse it, collect before_cursor only
    let full = full_input;
    let before_cursor: String = app.input[..app.cursor].iter().collect();
    let input_lines: Vec<&str> = full.split('\n').collect();
    let lines_before = before_cursor.split('\n').count();
    let cursor_line_idx = lines_before - 1;
    let cursor_col = before_cursor
        .rfind('\n')
        .map(|p| before_cursor.len() - p - 1)
        .unwrap_or(before_cursor.len());

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
                let cur_ch: String = if col < chars.len() {
                    chars[col].to_string()
                } else {
                    " ".to_string()
                };
                let after: String = if col < chars.len() {
                    chars[col + 1..].iter().collect()
                } else {
                    String::new()
                };
                // Append dim suggestion or placeholder after cursor (cursor line only)
                let mut spans = vec![
                    Span::styled(prompt, effective_prompt_style),
                    Span::styled(before, text_style),
                    Span::styled(cur_ch, cursor_style),
                    Span::styled(after, text_style),
                ];
                if show_placeholder {
                    spans.push(Span::styled("Message rustyclaw…", suggestion_style));
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

    f.render_widget(
        Paragraph::new(Text::from(render_lines)).wrap(Wrap { trim: false }),
        area,
    );
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
        let ctx_window = context_window_for_model(&app.model_short);
        let pct = app.cost_tracker.context_pct(ctx_window);
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

    // Right side: "● model-name" — clean, no token counts (matches rustyclaw)
    let right_text = format!("● {} ", app.model_short);
    let right_width = right_text.len() as u16;
    let left_width = area.width.saturating_sub(right_width);

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
    f.render_widget(
        Paragraph::new(Span::styled(right_text, Style::default().fg(tc.assistant))),
        right_area,
    );
}

/// Estimate context window size (tokens) for a model name.
fn context_window_for_model(model: &str) -> u64 {
    let m = model.to_lowercase();
    // Claude 4.x + 3.5 / 3.7 all ship 200k context windows today. If Anthropic
    // changes this we want the table — not one branch per model — to update.
    if ["opus", "sonnet", "haiku"].iter().any(|k| m.contains(k)) {
        200_000
    } else if ["gpt-4o", "gpt-4", "llama"].iter().any(|k| m.contains(k)) {
        128_000
    } else if m.contains("deepseek") {
        64_000
    } else if m.contains("mistral") {
        32_000
    } else if m.contains("gemma") {
        8_192
    } else {
        // Conservative default for unknown models — assumes modern Claude-class.
        200_000
    }
}

// ── Browse approval dialog ────────────────────────────────────────────────────

fn draw_browse_approval(f: &mut Frame, area: Rect, app: &App, tc: ThemeColors) {
    let Some(prompt) = &app.browse_approval else {
        return;
    };

    let popup_w = (area.width * 6 / 10).max(50).min(area.width);
    let popup_h = 12_u16.min(area.height);
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

    let mut lines = vec![
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
        Line::raw(""),
    ];
    lines.push(Line::from(vec![
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
    ]));

    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
        inner,
    );
}

// ── Permission dialog ─────────────────────────────────────────────────────────

fn draw_permission(f: &mut Frame, area: Rect, app: &App, tc: ThemeColors) {
    let Some(perm) = &app.pending_permission else {
        return;
    };

    let popup_w = (area.width * 7 / 10).max(50).min(area.width);
    let desc_lines = perm.description.lines().count() as u16 + 5;
    let popup_h = desc_lines.max(8).min(area.height);
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

    let mut lines = vec![Line::raw("")];
    for dl in perm.description.lines() {
        lines.push(Line::from(Span::styled(
            format!("  {dl}"),
            Style::default().fg(Color::White),
        )));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(vec![
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
    ]));

    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
        inner,
    );
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

    // Use pre-rendered markdown lines — computed once in Overlay::new(), not every frame
    let total = overlay.rendered.len();
    let visible = inner.height as usize;

    // For interactive overlays, highlight the selected item line.
    // Session list lines start with "  N. [" — the Nth item maps to selectable_ids[N-1].
    let selected_1based = if overlay.is_interactive() {
        overlay.selected + 1
    } else {
        0
    };

    // Auto-scroll to keep the selected item visible
    if selected_1based > 0 {
        let prefix = format!("{}.", selected_1based);
        if let Some(line_idx) = overlay.rendered.iter().position(|line| {
            let raw: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            raw.trim_start().starts_with(&prefix)
        }) {
            // Ensure the selected line is within the visible window
            if line_idx < overlay.scroll {
                overlay.scroll = line_idx;
            } else if line_idx >= overlay.scroll + visible {
                overlay.scroll = line_idx.saturating_sub(visible - 1);
            }
        }
    }

    overlay.scroll = overlay.scroll.min(total.saturating_sub(visible));
    let skip = overlay.scroll;

    let display: Vec<Line> = overlay
        .rendered
        .iter()
        .skip(skip)
        .take(visible)
        .cloned()
        .map(|mut line| {
            // Check if this rendered line starts with a list number matching the selected item
            if selected_1based > 0 {
                let raw: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                let trimmed = raw.trim_start();
                let prefix = format!("{}.", selected_1based);
                if trimmed.starts_with(&prefix) {
                    // Highlight the entire line
                    for span in &mut line.spans {
                        span.style = span
                            .style
                            .bg(Color::Rgb(50, 50, 80))
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD);
                    }
                }
            }
            line
        })
        .collect();

    f.render_widget(
        Paragraph::new(Text::from(display)).wrap(Wrap { trim: false }),
        inner,
    );
}

// ── AskUser dialog ────────────────────────────────────────────────────────────

fn draw_ask_user(f: &mut Frame, area: Rect, app: &App) {
    let Some(q) = &app.pending_user_question else {
        return;
    };

    let question_lines = q.question.lines().count() as u16;
    let popup_w = (area.width * 7 / 10).max(50).min(area.width);
    let popup_h = (question_lines + 7).max(8).min(area.height);
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

    let mut lines = vec![Line::raw("")];
    for ql in q.question.lines() {
        lines.push(Line::from(Span::styled(
            format!("  {ql}"),
            Style::default().fg(Color::White),
        )));
    }
    lines.push(Line::raw(""));

    // Render the text input row with cursor
    let before: String = q.input[..q.cursor].iter().collect();
    let rest: Vec<char> = q.input[q.cursor..].to_vec();
    let cursor_str = rest.first().map_or(" ", |_| " "); // block cursor
    let (cur_ch, after_str) = if rest.is_empty() {
        (" ".to_string(), String::new())
    } else {
        (rest[0].to_string(), rest[1..].iter().collect())
    };

    lines.push(Line::from(vec![
        Span::styled(
            "  > ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(before, Style::default().fg(Color::White)),
        Span::styled(cur_ch, Style::default().bg(Color::White).fg(Color::Black)),
        Span::styled(after_str, Style::default().fg(Color::White)),
    ]));

    let _ = cursor_str; // suppress unused warning

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "  Enter to send  ·  Esc to cancel",
        Style::default().fg(Color::DarkGray),
    )));

    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: true }),
        inner,
    );
}
