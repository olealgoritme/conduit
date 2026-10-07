//! The palette of `conduit setup`: one place for every style the view uses.
//!
//! Only ANSI named colours (they follow the user's terminal theme and work on
//! 16-colour terminals). One accent for titles, keys and the current item;
//! green/yellow/red only for ok/warn/to-do; faint only for text that is never
//! the only way to learn something. `NO_COLOR` keeps the modifiers (bold,
//! reversed) so the selection and the keys still stand out.

use ratatui::style::{Color, Modifier, Style};
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// Panel titles, the current step, the wordmark.
    pub title: Style,
    /// The key part of a `key action` hint.
    pub key: Style,
    /// Secondary text: separators, future steps, the tagline.
    pub dim: Style,
    pub ok: Style,
    pub warn: Style,
    pub fail: Style,
    /// The selected list row.
    pub selected: Style,
    /// A command line shown before it runs.
    pub command: Style,
    /// Borders of panels.
    pub border: Style,
    /// Borders of dialogs.
    pub dialog: Style,
    /// The call to action on the welcome page.
    pub cta: Style,
    /// Plain emphasis (headings, the step a row is about).
    pub strong: Style,
}

impl Theme {
    pub fn color() -> Theme {
        let accent = Color::Cyan;
        Theme {
            title: Style::new().fg(accent).add_modifier(Modifier::BOLD),
            key: Style::new().fg(accent).add_modifier(Modifier::BOLD),
            dim: Style::new().add_modifier(Modifier::DIM),
            ok: Style::new().fg(Color::Green),
            warn: Style::new().fg(Color::Yellow),
            fail: Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            selected: Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD),
            command: Style::new().add_modifier(Modifier::BOLD),
            border: Style::new().add_modifier(Modifier::DIM),
            dialog: Style::new().fg(accent),
            cta: Style::new()
                .fg(accent)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
            strong: Style::new().add_modifier(Modifier::BOLD),
        }
    }

    /// The same styles without any colour.
    pub fn plain() -> Theme {
        let p = |s: Style| Style {
            fg: None,
            bg: None,
            ..s
        };
        let c = Theme::color();
        Theme {
            title: p(c.title),
            key: p(c.key),
            dim: p(c.dim),
            ok: p(c.ok),
            warn: p(c.warn),
            fail: p(c.fail),
            selected: p(c.selected),
            command: p(c.command),
            border: p(c.border),
            dialog: p(c.dialog),
            cta: p(c.cta),
            strong: p(c.strong),
        }
    }

    /// `NO_COLOR` set (to anything, as `conduit trace` reads it) means plain.
    pub fn detect() -> &'static Theme {
        static T: OnceLock<Theme> = OnceLock::new();
        T.get_or_init(|| {
            if std::env::var_os("NO_COLOR").is_some() {
                Theme::plain()
            } else {
                Theme::color()
            }
        })
    }
}
