//! Semantic palette — every color the TUI uses, in one place, named by role
//! not by hue. Values follow the tokyonight family (the same palette lineage
//! most modern agent TUIs draw from) so the chrome reads as one coherent
//! surface instead of a stack of one-off ANSI picks.

use ratatui::style::Color;

/// tokyonight-night base colors.
pub mod base {
    use ratatui::style::Color;

    pub const BG: Color = Color::Rgb(26, 27, 38); // #1a1b26
    pub const BG_PANEL: Color = Color::Rgb(31, 34, 50); // tool output / code bg
    pub const BG_HIGHLIGHT: Color = Color::Rgb(41, 46, 66); // user prompt band
    pub const BG_SELECTED: Color = Color::Rgb(51, 58, 88); // selected block
    pub const FG: Color = Color::Rgb(192, 202, 245); // #c0caf5
    pub const FG_MUTED: Color = Color::Rgb(115, 122, 162); // dark5
    pub const FG_FAINT: Color = Color::Rgb(86, 95, 137); // comment
    pub const BLUE: Color = Color::Rgb(122, 162, 247); // #7aa2f7
    pub const CYAN: Color = Color::Rgb(125, 207, 255); // #7dcfff
    pub const MAGENTA: Color = Color::Rgb(187, 154, 247); // #bb9af7
    pub const GREEN: Color = Color::Rgb(158, 206, 106); // #9ece6a
    pub const YELLOW: Color = Color::Rgb(224, 175, 104); // #e0af68
    pub const ORANGE: Color = Color::Rgb(255, 158, 100); // #ff9e64
    pub const RED: Color = Color::Rgb(247, 118, 142); // #f7768e
}

/// Colors by role. Anything that paints picks from here.
pub struct Theme {
    /// primary text
    pub text: Color,
    /// secondary text (arg values, dim prose)
    pub muted: Color,
    /// hints, placeholders, separators
    pub faint: Color,
    /// your prompt: prefix + band
    pub user: Color,
    /// assistant marker / headings
    pub assistant: Color,
    /// reasoning stream
    pub thinking: Color,
    /// tool name in block headers
    pub tool: Color,
    /// running-tool indicator
    pub running: Color,
    /// success mark, allow option
    pub ok: Color,
    /// failure mark, deny option, error text
    pub err: Color,
    /// warnings, approval card accent
    pub warn: Color,
    /// user prompt full-width band
    pub band_bg: Color,
    /// tool output preview / fenced code background
    pub panel_bg: Color,
    /// scrollback-selected block background
    pub sel_bg: Color,
    /// left rail marking the selected block
    pub sel_rail: Color,
    /// inline code text
    pub code: Color,
    /// slash-menu / card highlight row
    pub hi: Color,
}

pub const THEME: Theme = Theme {
    text: base::FG,
    muted: base::FG_MUTED,
    faint: base::FG_FAINT,
    user: base::YELLOW,
    assistant: base::BLUE,
    thinking: base::FG_MUTED,
    tool: base::CYAN,
    running: base::ORANGE,
    ok: base::GREEN,
    err: base::RED,
    warn: base::YELLOW,
    band_bg: base::BG_HIGHLIGHT,
    panel_bg: base::BG_PANEL,
    sel_bg: base::BG_SELECTED,
    sel_rail: base::MAGENTA,
    code: base::CYAN,
    hi: base::YELLOW,
};

/// Legacy ConHost can't draw several of the pretty glyphs — probe once.
/// Windows Terminal / WezTerm / VS Code all set one of these env vars.
pub fn legacy_glyphs() -> bool {
    cfg!(windows)
        && std::env::var_os("WT_SESSION").is_none()
        && std::env::var_os("TERM_PROGRAM").is_none()
        && std::env::var_os("WEZTERM_PANE").is_none()
}

/// Prompt marker in front of user bands and the composer.
pub fn prompt_glyph() -> &'static str {
    if legacy_glyphs() {
        "> "
    } else {
        "❯ "
    }
}
