//! Sparse Astra stars on the existing composer surface, fading with the terminal's colors.
//! `enabled_foreground` owns eligibility, including late-arriving terminal colors.
//! Rendering owns frame scheduling, so hidden composers do not keep animating. Stars are painted
//! into otherwise blank cell backgrounds while keeping their space symbols, so terminal selection
//! and clipboard copies contain only the composer's actual text.

use std::sync::LazyLock;
use std::time::Duration;
use std::time::Instant;

use codex_config::types::Tui;
use ratatui::buffer::Buffer;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Rect;
use ratatui::style::Color;
use regex_lite::Regex;
use unicode_width::UnicodeWidthStr;

use super::ChatComposer;
use super::popup_state::ActivePopup;
use crate::bottom_pane::BottomPane;
use crate::color::blend;
use crate::style::user_message_bg_rgb;
use crate::terminal_palette::StdoutColorLevel;
use crate::terminal_palette::default_bg;
use crate::terminal_palette::default_fg;
use crate::terminal_palette::effective_stdout_color_level;
use crate::terminal_palette::rgb_color;

const FRAME_TICK: Duration = Duration::from_millis(150);
const STAR_BACKGROUND_ALPHA: f32 = 0.25;
static ASTRA_MODEL: LazyLock<Regex> = LazyLock::new(|| match Regex::new(r"(?i)\bastra\b") {
    Ok(regex) => regex,
    Err(error) => panic!("invalid Astra model regex: {error}"),
});

pub(super) struct Sparkle {
    model: String,
    whimsy: bool,
    animations: bool,
    started: Instant,
}

impl Sparkle {
    fn enabled_foreground(&self) -> Option<(u8, u8, u8)> {
        if !self.whimsy
            || !self.animations
            || !ASTRA_MODEL.is_match(&self.model)
            || effective_stdout_color_level() != StdoutColorLevel::TrueColor
        {
            return None;
        }
        // Read the cached palette here: Windows may populate it after widget creation.
        default_fg()
    }
}

impl BottomPane {
    pub(crate) fn set_astra_sparkle(&mut self, model: &str, settings: &Tui) {
        let started = self
            .composer
            .astra_sparkle
            .as_ref()
            .map_or_else(Instant::now, |sparkle| sparkle.started);
        self.composer.astra_sparkle = Some(Sparkle {
            model: model.to_owned(),
            whimsy: settings.whimsy,
            animations: settings.animations,
            started,
        });
    }
}

impl ChatComposer {
    pub(super) fn render_sparkle(&self, area: Rect, cursor: Option<(u16, u16)>, buf: &mut Buffer) {
        if area.is_empty() || !matches!(self.popups.active, ActivePopup::None) {
            return;
        }
        if let Some(sparkle) = &self.astra_sparkle
            && let Some(foreground) = sparkle.enabled_foreground()
        {
            let Some(terminal_bg) = default_bg() else {
                return;
            };
            render_stars(
                area,
                cursor,
                sparkle.started.elapsed(),
                foreground,
                user_message_bg_rgb(terminal_bg),
                buf,
            );
            if let Some(requester) = &self.frame_requester {
                requester.schedule_frame_in(FRAME_TICK);
            }
        }
    }
}

fn render_stars(
    area: Rect,
    cursor: Option<(u16, u16)>,
    elapsed: Duration,
    foreground: (u8, u8, u8),
    base_background: (u8, u8, u8),
    buf: &mut Buffer,
) {
    let time = elapsed.as_secs_f32();
    for y in area.y..area.bottom() {
        let mut occupied_until = area.x;
        for x in area.x..area.right() {
            let cell = &buf[(x, y)];
            if x < occupied_until {
                continue;
            }
            if cell.symbol() != " " {
                occupied_until = x.saturating_add(cell.symbol().width() as u16);
                continue;
            }
            // Preserve the cursor, selections, and pixels from the effort bursts.
            if cursor == Some((x, y))
                || !cell.modifier.is_empty()
                || cell.diff_option != CellDiffOption::None
            {
                continue;
            }
            let Color::Rgb(r, g, b) = cell.bg else {
                continue;
            };
            // Effort bursts and other overlays own their tinted backgrounds; leave those pixels
            // untouched so the sparkle layer cannot wash out an active effect.
            if (r, g, b) != base_background {
                continue;
            }
            // A stable coordinate hash gives each star its own period and phase.
            let mut hash = u64::from(y - area.y) * 65537 + u64::from(x - area.x);
            hash = (hash ^ (hash >> 16)).wrapping_mul(0x45d9f3b);
            hash = (hash ^ (hash >> 16)).wrapping_mul(0x45d9f3b);
            hash ^= hash >> 16;
            if hash % 5 != 0 {
                continue;
            }
            let phase =
                (time / (4.0 + (hash % 31) as f32 / 10.0) + (hash % 997) as f32 / 997.0).fract();
            let brightness = (phase * std::f32::consts::PI).sin().powi(12) * 0.55;
            if brightness < 0.04 {
                continue;
            }
            // Keep the cell's space symbol intact: terminal selection copies cell symbols, so
            // using a decorative glyph here would leak stars into copied composer text.
            buf[(x, y)].set_bg(rgb_color(blend(
                foreground,
                (r, g, b),
                brightness * STAR_BACKGROUND_ALPHA,
            )));
        }
    }
}

#[cfg(test)]
#[path = "sparkle_tests.rs"]
mod tests;
