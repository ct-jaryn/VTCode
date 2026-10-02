use anstyle::{Color as AnsiColorEnum, Style as AnsiStyle};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::tui::ui::theme;

// Re-export from commons so existing consumers don't break.
pub use vtcode_commons::ui_protocol::{convert_style, theme_from_color_fields};

use super::types::{InlineTextStyle, InlineTheme};

pub fn theme_from_styles(styles: &theme::ThemeStyles) -> InlineTheme {
    theme_from_color_fields(
        styles.foreground,
        styles.background,
        styles.primary,
        styles.secondary,
        styles.tool,
        styles.tool_detail,
        styles.pty_output,
        styles.error,
        styles.warning,
    )
}

pub(crate) fn measure_text_width(text: &str) -> u16 {
    UnicodeWidthStr::width(text) as u16
}

/// Convert anstyle Color to ratatui Color.
///
/// Delegates to `crate::design::color::anstyle_to_ratatui_color` which
/// provides the correct mapping (fixing the Magenta bug).
pub(crate) fn ratatui_color_from_ansi(color: AnsiColorEnum) -> Color {
    crate::design::color::anstyle_to_ratatui_color(color)
}

/// Parse a hex color string (e.g., "#D99A4E") to a ratatui Color.
/// Returns None if the string is invalid or cannot be parsed.
pub(crate) use crate::design::color::hex_to_ratatui_color;

/// Get the agent color style from an optional color token.
///
/// The token may be a primary-agent mode name (`"build"`), a standard ANSI hue
/// name (`"green"`), or a `#rrggbb` hex string. It is resolved theme-aware via
/// the design system so the badge stays legible on both dark and light
/// terminals, with `fallback_color` used when the token is empty or unknown.
pub(crate) fn agent_color_style(color: Option<&str>, fallback_color: Color) -> Style {
    let light = matches!(
        vtcode_commons::ansi_capabilities::detect_color_scheme(),
        vtcode_commons::ansi_capabilities::ColorScheme::Light
    );
    let color = color
        .map(|c| crate::design::color::resolve_agent_color(c, fallback_color, light))
        .unwrap_or(fallback_color);
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

pub(crate) fn ratatui_style_from_inline(style: &InlineTextStyle, fallback: Option<AnsiColorEnum>) -> Style {
    crate::design::style::inline_text_style_to_ratatui(style.color, style.bg_color, style.effects, fallback)
}

/// PTY output style helper: keep configured colors and suppress bold.
pub(crate) fn ratatui_pty_style_from_inline(style: &InlineTextStyle, fallback: Option<AnsiColorEnum>) -> Style {
    ratatui_style_from_inline(style, fallback).remove_modifier(Modifier::BOLD)
}

/// Convert an `anstyle::Style` directly to a `ratatui::style::Style`.
pub(crate) fn ratatui_style_from_ansi(style: AnsiStyle) -> Style {
    crate::design::style::anstyle_to_ratatui_style(style)
}

/// Shimmer sweep geometry shared with `tui-shimmer`.
///
/// `SHIMMER_PAD`/`SHIMMER_HALF_WIDTH` mirror the upstream constants so a
/// mode-highlighted sweep stays positionally in sync with crate-driven sweeps
/// (e.g. the transcript indicator row) at the same shared phase.
const SHIMMER_PAD: isize = 10;
const SHIMMER_HALF_WIDTH: usize = 5;

/// Build a shimmer sweep over `text`, optionally highlighted in `highlight`.
///
/// With `None` this delegates to `tui-shimmer` unchanged (modeless behavior).
/// With `Some` the base text keeps `base_style` untouched while the moving
/// band renders in the highlight hue — no RGB math, so it stays deterministic
/// on truecolor and ANSI16 terminals alike.
pub(crate) fn mode_shimmer_spans(text: &str, base: Style, highlight: Option<Color>, phase: f32) -> Vec<Span<'static>> {
    let Some(highlight) = highlight else {
        return tui_shimmer::shimmer_spans_with_style_at_phase(text, base, phase);
    };
    let char_count = text.chars().count();
    if char_count == 0 {
        return Vec::new();
    }

    let phase = phase.rem_euclid(1.0);
    let period = char_count as isize + SHIMMER_PAD * 2;
    let pos = (phase * period as f32) as isize;

    let mut spans = Vec::with_capacity(char_count);
    let mut buffer = String::new();
    let mut current_style: Option<Style> = None;

    for (index, ch) in text.chars().enumerate() {
        let dist = (index as isize + SHIMMER_PAD - pos).unsigned_abs() as f32;
        let half = SHIMMER_HALF_WIDTH as f32;
        // Cosine falloff matching the upstream intensity curve.
        let intensity = if dist <= half {
            0.5 * (1.0 + (std::f32::consts::PI * dist / half).cos())
        } else {
            0.0
        };

        let mut style = base;
        if intensity >= 0.6 {
            style = style.fg(highlight).add_modifier(Modifier::BOLD);
        } else if intensity > 0.0 {
            style = style.fg(highlight);
        }

        let same_style = current_style.as_ref().is_some_and(|current| current == &style);
        if !same_style {
            if let Some(prev_style) = current_style.take() {
                if !buffer.is_empty() {
                    spans.push(Span::styled(buffer, prev_style));
                    buffer = String::new();
                }
            }
            current_style = Some(style);
        }
        buffer.push(ch);
    }

    if let Some(final_style) = current_style {
        if !buffer.is_empty() {
            spans.push(Span::styled(buffer, final_style));
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::design::color::resolve_agent_color;
    use vtcode_config::constants::ui::{AGENT_COLOR_AUTO, AGENT_COLOR_BUILD, AGENT_COLOR_DUCK, AGENT_COLOR_PLAN};

    #[test]
    fn agent_color_style_applies_mode_color_with_bold() {
        let fallback = Color::LightMagenta;
        let style = agent_color_style(Some(AGENT_COLOR_BUILD), fallback);
        // The exact variant depends on the detected terminal scheme, but it must
        // be a concrete (non-fallback) standard color and always bold.
        assert_ne!(style.fg, Some(fallback));
        assert!(style.add_modifier.contains(Modifier::BOLD));

        for hue in [AGENT_COLOR_BUILD, AGENT_COLOR_AUTO, AGENT_COLOR_PLAN, AGENT_COLOR_DUCK] {
            let s = agent_color_style(Some(hue), fallback);
            assert!(s.add_modifier.contains(Modifier::BOLD));
            assert!(s.fg.is_some());
        }
    }

    #[test]
    fn agent_color_style_is_theme_aware_and_distinct_per_mode() {
        let fallback = Color::LightMagenta;
        // Build is a fixed hex token: identical on both appearances.
        let build = Color::Rgb(0x73, 0xA1, 0x8E);
        // On a dark terminal each hue resolves to its bright variant.
        let dark = [
            resolve_agent_color(AGENT_COLOR_BUILD, fallback, false),
            resolve_agent_color(AGENT_COLOR_AUTO, fallback, false),
            resolve_agent_color(AGENT_COLOR_PLAN, fallback, false),
            resolve_agent_color(AGENT_COLOR_DUCK, fallback, false),
        ];
        assert_eq!(dark, [build, Color::LightGreen, Color::LightBlue, Color::LightMagenta]);
        // On a light terminal each hue resolves to its base variant.
        let light = [
            resolve_agent_color(AGENT_COLOR_BUILD, fallback, true),
            resolve_agent_color(AGENT_COLOR_AUTO, fallback, true),
            resolve_agent_color(AGENT_COLOR_PLAN, fallback, true),
            resolve_agent_color(AGENT_COLOR_DUCK, fallback, true),
        ];
        assert_eq!(light, [build, Color::Green, Color::Blue, Color::Magenta]);
        // The four modes must remain visually distinct in both appearances.
        assert_eq!(dark.iter().collect::<std::collections::HashSet<_>>().len(), 4);
        assert_eq!(light.iter().collect::<std::collections::HashSet<_>>().len(), 4);
    }

    #[test]
    fn agent_color_style_accepts_raw_hue_names_and_hex() {
        let fallback = Color::LightMagenta;
        // Raw standard ANSI hue name (as emitted by the plan-approval overlay).
        assert_eq!(resolve_agent_color("green", fallback, false), Color::LightGreen);
        assert_eq!(resolve_agent_color("blue", fallback, true), Color::Blue);
        // Legacy hex still resolves.
        assert_eq!(resolve_agent_color("#FF0000", fallback, false), Color::Rgb(255, 0, 0));
    }

    #[test]
    fn agent_color_style_falls_back_when_missing_or_invalid() {
        let fallback = Color::LightMagenta;
        let missing = agent_color_style(None, fallback);
        assert_eq!(missing.fg, Some(fallback));
        assert!(missing.add_modifier.contains(Modifier::BOLD));

        let invalid = agent_color_style(Some("not-a-color"), fallback);
        assert_eq!(invalid.fg, Some(fallback));
    }

    fn sweep_text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn mode_sweep_delegates_without_highlight() {
        let spans = mode_shimmer_spans("Running", Style::default(), None, 0.25);
        assert!(!spans.is_empty());
        assert_eq!(sweep_text(&spans), "Running");
    }

    #[test]
    fn mode_sweep_yields_no_spans_for_empty_text() {
        assert!(mode_shimmer_spans("", Style::default(), Some(Color::Red), 0.5).is_empty());
    }

    #[test]
    fn mode_sweep_centers_highlight_on_band_and_preserves_base() {
        // 8 chars -> period 28; phase 11/28 parks the band near 'b' (index 1).
        // Assertions hold for either truncation side of the float product.
        let base = Style::default();
        let spans = mode_shimmer_spans("abcdefgh", base, Some(Color::Red), 11.0 / 28.0);
        assert_eq!(sweep_text(&spans), "abcdefgh");

        for ch in ['a', 'b'] {
            let span = spans.iter().find(|span| span.content.as_ref().contains(ch)).expect("band span");
            assert_eq!(span.style.fg, Some(Color::Red), "band should carry the highlight hue");
        }
        // 'h' sits outside the half-width band: base style untouched.
        let edge = spans
            .iter()
            .find(|span| span.content.as_ref().contains('h'))
            .expect("edge span");
        assert_eq!(edge.style.fg, base.fg);
    }
}
