use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::CancellationEvent;
use crate::bottom_pane::bottom_pane_view::BottomPaneView;
use crate::bottom_pane::bottom_pane_view::ViewCompletion;
use crate::realtime_voice_effects::VoiceEffectPreset;
use crate::render::renderable::Renderable;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

#[cfg(test)]
#[path = "realtime_voice_tuner_tests.rs"]
mod tests;

const CONTROL_NAMES: [&str; 6] = [
    "Output gain",
    "Pitch shift",
    "Formant shift",
    "Saturation",
    "Ring-mod mix",
    "Reverb mix",
];

pub(crate) struct RealtimeVoiceTuner {
    preset: VoiceEffectPreset,
    original_preset: VoiceEffectPreset,
    selected: usize,
    bypass: bool,
    completion: Option<ViewCompletion>,
    app_event_tx: AppEventSender,
}

impl RealtimeVoiceTuner {
    pub(crate) fn new(preset: VoiceEffectPreset, app_event_tx: AppEventSender) -> Self {
        Self {
            original_preset: preset.clone(),
            preset,
            selected: 0,
            bypass: false,
            completion: None,
            app_event_tx,
        }
    }

    fn adjust(&mut self, direction: f32) {
        match self.selected {
            0 => {
                self.preset.output_gain_db =
                    (self.preset.output_gain_db + direction).clamp(-24.0, 12.0)
            }
            1 => {
                self.preset.pitch_shift_semitones =
                    (self.preset.pitch_shift_semitones + direction).clamp(-12.0, 12.0)
            }
            2 => {
                self.preset.formant_shift_semitones =
                    (self.preset.formant_shift_semitones + direction).clamp(-6.0, 6.0)
            }
            3 => {
                self.preset.saturation = (self.preset.saturation + direction * 0.05).clamp(0.0, 1.0)
            }
            4 => {
                self.preset.ring_mod_mix =
                    (self.preset.ring_mod_mix + direction * 0.05).clamp(0.0, 1.0)
            }
            5 => {
                self.preset.reverb_mix = (self.preset.reverb_mix + direction * 0.05).clamp(0.0, 1.0)
            }
            _ => unreachable!(),
        }
        self.app_event_tx.send(AppEvent::RealtimeVoiceEffectUpdate {
            preset: self.preset.clone(),
            persist: false,
            bypass: self.bypass,
        });
    }

    fn value(&self, index: usize) -> String {
        match index {
            0 => format!("{:.1} dB", self.preset.output_gain_db),
            1 => format!("{:.1} semitones", self.preset.pitch_shift_semitones),
            2 => format!("{:.1} semitones", self.preset.formant_shift_semitones),
            3 => format!("{:.2}", self.preset.saturation),
            4 => format!("{:.2}", self.preset.ring_mod_mix),
            5 => format!("{:.2}", self.preset.reverb_mix),
            _ => unreachable!(),
        }
    }

    fn restore_original(&self) {
        self.app_event_tx.send(AppEvent::RealtimeVoiceEffectUpdate {
            preset: self.original_preset.clone(),
            persist: false,
            bypass: false,
        });
    }
}

impl BottomPaneView for RealtimeVoiceTuner {
    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc => {
                self.restore_original();
                self.completion = Some(ViewCompletion::Cancelled);
            }
            KeyCode::Up | KeyCode::Char('k') => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = (self.selected + 1).min(CONTROL_NAMES.len() - 1)
            }
            KeyCode::Left | KeyCode::Char('-') => self.adjust(-1.0),
            KeyCode::Right | KeyCode::Char('+') | KeyCode::Char('=') => self.adjust(1.0),
            KeyCode::Char('b') => {
                self.bypass = !self.bypass;
                self.app_event_tx.send(AppEvent::RealtimeVoiceEffectUpdate {
                    preset: self.preset.clone(),
                    persist: false,
                    bypass: self.bypass,
                });
            }
            KeyCode::Char('s') => {
                self.app_event_tx.send(AppEvent::RealtimeVoiceEffectUpdate {
                    preset: self.preset.clone(),
                    persist: true,
                    bypass: self.bypass,
                });
            }
            KeyCode::Enter => self.completion = Some(ViewCompletion::Accepted),
            _ => {}
        }
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.restore_original();
        self.completion = Some(ViewCompletion::Cancelled);
        CancellationEvent::Handled
    }

    fn is_complete(&self) -> bool {
        self.completion.is_some()
    }
    fn completion(&self) -> Option<ViewCompletion> {
        self.completion
    }
}

impl Renderable for RealtimeVoiceTuner {
    fn desired_height(&self, _width: u16) -> u16 {
        10
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let mut lines = vec![Line::from(vec!["  GPT-Live voice tuner".bold()])];
        for (index, name) in CONTROL_NAMES.iter().enumerate() {
            let marker = if index == self.selected { "›" } else { " " };
            let style = if index == self.selected {
                Span::from(*name).cyan()
            } else {
                Span::from(*name)
            };
            lines.push(Line::from(vec![
                Span::from(format!(" {marker} ")),
                style,
                Span::from(format!(": {}", self.value(index))),
            ]));
        }
        lines.push(Line::from(
            format!(
                "  {}  arrows: select/adjust  b: A/B  s: save  enter: keep  esc: cancel",
                if self.bypass { "BYPASS" } else { "LIVE" }
            )
            .dim(),
        ));
        Paragraph::new(lines).render(area, buf);
    }
}
