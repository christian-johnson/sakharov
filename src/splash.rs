use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::app::App;

// ANSI Shadow figlet font — "sakharov" (66 display columns, 6 rows)
const BANNER: &[&str] = &[
    "███████╗ █████╗ ██╗  ██╗██╗  ██╗ █████╗ ██████╗  ██████╗ ██╗   ██╗",
    "██╔════╝██╔══██╗██║ ██╔╝██║  ██║██╔══██╗██╔══██╗██╔═══██╗██║   ██║",
    "███████╗███████║█████╔╝ ███████║███████║██████╔╝██║   ██║██║   ██║",
    "╚════██║██╔══██║██╔═██╗ ██╔══██║██╔══██║██╔══██╗██║   ██║╚██╗ ██╔╝",
    "███████║██║  ██║██║  ██╗██║  ██║██║  ██║██║  ██╗╚██████╔╝ ╚████╔╝ ",
    "╚══════╝╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═╝ ╚═════╝   ╚═══╝  ",
];
const BANNER_W: u16 = 66;

struct Action {
    label: &'static str,
    command_name: &'static str,
    fallback: &'static str,
}

const ACTIONS: &[Action] = &[
    Action { label: "open file",       command_name: "open-file-picker",     fallback: "C-o"          },
    Action { label: "command palette", command_name: "open-command-palette", fallback: "SPC"          },
    Action { label: "grep project",    command_name: "grep-project",         fallback: "C-g"          },
    Action { label: "open config",     command_name: "open-config",          fallback: ":open-config" },
    Action { label: "scratch buffer",  command_name: "switch-to-scratch",    fallback: ":scratch"     },
    Action { label: "quit",            command_name: "quit",                 fallback: ":q"           },
];

const LABEL_COL: usize = 18; // right-pad labels to this width
const FOOTER: &str = "press any key to start";

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let banner_rows = BANNER.len() as u16;
    let action_rows = ACTIONS.len() as u16;
    // banner + gap + actions + gap + footer
    let content_h = banner_rows + 2 + action_rows + 2 + 1;

    let top = if area.height > content_h {
        area.y + (area.height - content_h) / 2
    } else {
        area.y
    };

    let banner_x = if area.width > BANNER_W {
        area.x + (area.width - BANNER_W) / 2
    } else {
        area.x
    };

    let banner_color = crate::theme::mode_color(
        &crate::mode::Mode::Normal,
        &app.config.theme.modes,
    );

    // ── Banner ──────────────────────────────────────────────────────────────
    for (i, row) in BANNER.iter().enumerate() {
        let y = top + i as u16;
        if y >= area.y + area.height {
            break;
        }
        let w = area.width.saturating_sub(banner_x - area.x);
        frame.render_widget(
            Paragraph::new(*row).style(Style::default().fg(banner_color)),
            Rect { x: banner_x, y, width: w, height: 1 },
        );
    }

    // ── Actions ─────────────────────────────────────────────────────────────
    let actions_y = top + banner_rows + 2;

    // Actions are slightly indented relative to the banner left edge.
    let actions_x = banner_x.saturating_add(2);

    for (i, action) in ACTIONS.iter().enumerate() {
        let y = actions_y + i as u16;
        if y >= area.y + area.height {
            break;
        }

        let hint = app
            .keymap
            .hint_for_command(action.command_name)
            .unwrap_or_else(|| action.fallback.to_string());

        // "  ▸  label ................  hint"
        let dots = if action.label.len() < LABEL_COL {
            ".".repeat(LABEL_COL - action.label.len())
        } else {
            String::new()
        };

        let line = Line::from(vec![
            Span::styled("  ▸  ", Style::default().fg(Color::DarkGray)),
            Span::styled(action.label, Style::default().fg(Color::White)),
            Span::styled(format!(" {} ", dots), Style::default().fg(Color::DarkGray)),
            Span::styled(hint, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        ]);

        let w = area.width.saturating_sub(actions_x - area.x);
        frame.render_widget(
            Paragraph::new(line),
            Rect { x: actions_x, y, width: w, height: 1 },
        );
    }

    // ── Footer ───────────────────────────────────────────────────────────────
    let footer_y = actions_y + action_rows + 2;
    if footer_y < area.y + area.height {
        let fw = FOOTER.len() as u16;
        let footer_x = if area.width > fw {
            area.x + (area.width - fw) / 2
        } else {
            area.x
        };
        let w = area.width.saturating_sub(footer_x - area.x);
        frame.render_widget(
            Paragraph::new(FOOTER).style(Style::default().fg(Color::DarkGray)),
            Rect { x: footer_x, y: footer_y, width: w, height: 1 },
        );
    }
}
