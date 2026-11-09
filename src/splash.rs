use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
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

/// One dashboard row: what it does, and the command that does it.  The key
/// (or `:` name) beside it comes from the keymap and the command table, the
/// same as the palette's labels, so the two always agree.
struct Action {
    label: &'static str,
    command_name: &'static str,
}

const ACTIONS: &[Action] = &[
    Action { label: "open file",       command_name: "open-file-picker"     },
    Action { label: "command palette", command_name: "open-command-palette" },
    Action { label: "grep project",    command_name: "grep-project"         },
    Action { label: "open config",     command_name: "open-config"          },
    Action { label: "scratch buffer",  command_name: "switch-to-scratch"    },
    Action { label: "quit",            command_name: "quit"                 },
];

const LABEL_COL: usize = 18; // right-pad labels to this width
const FOOTER: &str = "press any key to start";

pub fn render(frame: &mut Frame, area: Rect, app: &App) {
    let th = crate::theme::active();
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

    let banner_color = crate::theme::mode_color(&crate::mode::Mode::Normal);

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

        let hint = app.keymap.hint(crate::keymap::Layer::Normal, action.command_name);

        // "  ▸  label ................  hint"
        let dots = if action.label.len() < LABEL_COL {
            ".".repeat(LABEL_COL - action.label.len())
        } else {
            String::new()
        };

        let line = Line::from(vec![
            Span::styled("  ▸  ", Style::default().fg(th.dim)),
            Span::styled(action.label, Style::default().fg(th.fg())),
            Span::styled(format!(" {} ", dots), Style::default().fg(th.dim)),
            Span::styled(hint, Style::default().fg(th.info).add_modifier(Modifier::BOLD)),
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
            Paragraph::new(FOOTER).style(Style::default().fg(th.dim)),
            Rect { x: footer_x, y: footer_y, width: w, height: 1 },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every dashboard row names a real command, findable in the palette
    /// under the name the row shows.
    #[test]
    fn every_action_is_a_palette_command() {
        let palette = crate::command::Command::palette_entries();
        for action in ACTIONS {
            assert!(
                palette.iter().any(|(n, _)| *n == action.command_name),
                "{:?} is not a palette command",
                action.command_name
            );
        }
    }

    #[test]
    fn rows_show_the_key_or_the_shortest_colon_name() {
        let keymap = crate::keymap::Keymap::default_bindings();
        let hint = |name| keymap.hint(crate::keymap::Layer::Normal, name);
        assert_eq!(hint("open-file-picker"), "C-o");
        assert_eq!(hint("switch-to-scratch"), ":scratch");
        assert_eq!(hint("open-config"), ":config");
        assert_eq!(hint("quit"), ":q");
    }
}
