#![allow(clippy::enum_glob_use)]

use cursive::theme::BaseColor::*;
use cursive::theme::Color::*;
use cursive::theme::PaletteColor::*;
use cursive::theme::*;
use log::warn;

use crate::config::ConfigTheme;

/// Get the given color from the given [ConfigTheme]. The first argument is the [ConfigTheme] to get
/// the color out of. The second argument is the name of the color to get and is an identifier. The
/// third argument is a [Color] that is used as the default when no color can be parsed from the
/// provided [ConfigTheme].
///
/// # Examples
///
/// ```rust
/// load_color!(config_theme, background, TerminalDefault)
/// load_color!(config_theme, primary, TerminalDefault)
/// ```
macro_rules! load_color {
    ( $theme: expr_2021, $member: ident, $default: expr_2021 ) => {
        $theme
            .as_ref()
            .and_then(|t| t.$member.clone())
            .and_then(|c| Color::parse(c.as_ref()))
            .unwrap_or_else(|| {
                warn!(
                    "Failed to parse color in \"{}\", falling back to default",
                    stringify!($member)
                );
                $default
            })
    };
}

/// Parse `s` as a [PaletteColor] name (case-insensitive, ignoring `-`/`_`),
/// resolving it against `palette`, or fall back to [Color::parse].
///
/// Valid palette names: Background, Shadow, View, Primary, Secondary, Tertiary,
/// TitlePrimary, TitleSecondary, Highlight, HighlightInactive, HighlightText.
fn resolve_color(s: &str, palette: &Palette) -> Option<Color> {
    match s.to_lowercase().replace(['-', '_'], "").as_str() {
        "background" => Some(palette[Background]),
        "shadow" => Some(palette[Shadow]),
        "view" => Some(palette[View]),
        "primary" => Some(palette[Primary]),
        "secondary" => Some(palette[Secondary]),
        "tertiary" => Some(palette[Tertiary]),
        "titleprimary" => Some(palette[TitlePrimary]),
        "titlesecondary" => Some(palette[TitleSecondary]),
        "highlight" => Some(palette[Highlight]),
        "highlightinactive" => Some(palette[HighlightInactive]),
        "highlighttext" => Some(palette[HighlightText]),
        _ => Color::parse(s),
    }
}

/// Create a [cursive::theme::Theme] from `theme_cfg`.
pub fn load(theme_cfg: &Option<ConfigTheme>) -> Theme {
    let mut palette = Palette::default();
    let borders = BorderStyle::Simple;

    palette[Background] = load_color!(theme_cfg, background, TerminalDefault);
    palette[View] = load_color!(theme_cfg, background, TerminalDefault);
    palette[Primary] = load_color!(theme_cfg, primary, TerminalDefault);
    palette[Secondary] = load_color!(theme_cfg, secondary, Dark(Blue));
    palette[TitlePrimary] = load_color!(theme_cfg, title, Dark(Red));
    palette[HighlightText] = load_color!(theme_cfg, highlight, Dark(White));
    palette[Highlight] = load_color!(theme_cfg, highlight_bg, Dark(Red));
    palette[HighlightInactive] = load_color!(theme_cfg, highlight_inactive_bg, Dark(Blue));
    palette.set_color("playing", load_color!(theme_cfg, playing, Dark(Blue)));
    palette.set_color(
        "playing_selected",
        load_color!(theme_cfg, playing_selected, Light(Blue)),
    );
    palette.set_color(
        "playing_bg",
        load_color!(theme_cfg, playing_bg, TerminalDefault),
    );
    palette.set_color("error", load_color!(theme_cfg, error, TerminalDefault));
    palette.set_color("error_bg", load_color!(theme_cfg, error_bg, Dark(Red)));
    palette.set_color(
        "statusbar_progress",
        load_color!(theme_cfg, statusbar_progress, Dark(Blue)),
    );
    palette.set_color(
        "statusbar_progress_bg",
        load_color!(theme_cfg, statusbar_progress_bg, Light(Black)),
    );
    palette.set_color("statusbar", load_color!(theme_cfg, statusbar, Dark(Yellow)));
    palette.set_color(
        "statusbar_bg",
        load_color!(theme_cfg, statusbar_bg, TerminalDefault),
    );
    palette.set_color("cmdline", load_color!(theme_cfg, cmdline, TerminalDefault));
    palette.set_color(
        "cmdline_bg",
        load_color!(theme_cfg, cmdline_bg, TerminalDefault),
    );
    palette.set_color(
        "search_match",
        load_color!(theme_cfg, search_match, Light(Red)),
    );

    // Dropdown colors are resolved after the main palette is built so they can
    // reference PaletteColor names (e.g. "Highlight") in addition to hex/named
    // values.  Unset fields inherit the matching standard palette entry.
    let c = theme_cfg.as_ref().and_then(|t| t.dropdown_fg.as_deref())
        .and_then(|s| resolve_color(s, &palette))
        .unwrap_or_else(|| palette[Primary]);
    palette.set_color("dropdown_fg", c);

    let c = theme_cfg.as_ref().and_then(|t| t.dropdown_bg.as_deref())
        .and_then(|s| resolve_color(s, &palette))
        .unwrap_or_else(|| palette[HighlightInactive]);
    palette.set_color("dropdown_bg", c);

    let c = theme_cfg.as_ref().and_then(|t| t.dropdown_focused_fg.as_deref())
        .and_then(|s| resolve_color(s, &palette))
        .unwrap_or_else(|| palette[HighlightText]);
    palette.set_color("dropdown_focused_fg", c);

    let c = theme_cfg.as_ref().and_then(|t| t.dropdown_focused_bg.as_deref())
        .and_then(|s| resolve_color(s, &palette))
        .unwrap_or_else(|| palette[Highlight]);
    palette.set_color("dropdown_focused_bg", c);

    let c = theme_cfg.as_ref().and_then(|t| t.dropdown_border.as_deref())
        .and_then(|s| resolve_color(s, &palette))
        .unwrap_or_else(|| palette[TitleSecondary]);
    palette.set_color("dropdown_border", c);

    Theme {
        shadow: false,
        palette,
        borders,
    }
}
