//! The verdict banner.
//!
//! A fixed footer that never scrolls and never competes for attention. It will
//! be read by a tired person in a dim room who is about to make an irreversible
//! decision, so it is the largest thing on screen, it carries a glyph as well as
//! a colour, and the whole band is washed in the state's hue so the answer is
//! legible from across the table before any word is read.
//!
//! The failure states get the same treatment as the good one, with the specific
//! diagnosis underneath -- never a generic "verification failed".

use egui::{Align, Color32, CornerRadius, Layout, RichText, Stroke, Ui};

use crate::engine::telemetry::Phase;
use crate::engine::verdict::{Verdict, VerdictReport};

use super::theme;

/// The band's hue.
///
/// The two middle tiers deliberately do *not* get amber. A run that did
/// everything the hardware allows must not look like a run with a failing drive
/// in it -- that is the whole reason they were split apart. They take the
/// destination hue instead, which reads as information rather than alarm and is
/// already established elsewhere in the window as "this is about the drives".
pub fn colour(state: Verdict) -> Color32 {
    match state {
        Verdict::SafeToFormat => theme::OK,
        Verdict::VerifiedOneSource | Verdict::VerifiedOneCopy => theme::DEST,
        Verdict::VerifiedDoNotFormat => theme::WARN,
        Verdict::Failed => theme::ERR,
    }
}

/// Colour is never the only signal, so each state also gets its own mark.
///
/// Roughly one man in twelve cannot separate the green from the red, and this
/// band is the last thing read before an irreversible decision. The glyph and
/// the headline both have to carry the answer on their own.
pub fn glyph(state: Verdict) -> &'static str {
    match state {
        Verdict::SafeToFormat => theme::glyphs().ok,
        Verdict::VerifiedOneSource | Verdict::VerifiedOneCopy => theme::glyphs().info,
        Verdict::VerifiedDoNotFormat => theme::glyphs().warn,
        Verdict::Failed => theme::glyphs().err,
    }
}

/// The mark for a phase that has not produced a verdict yet.
fn phase_glyph(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => theme::glyphs().phase_idle,
        Phase::Verify => theme::glyphs().phase_verify,
        Phase::Done => theme::glyphs().phase_idle,
        _ => theme::glyphs().phase_running,
    }
}

fn phase_words(phase: Phase) -> &'static str {
    match phase {
        Phase::Idle => "IDLE",
        Phase::Done => "DONE",
        Phase::Verify => "VERIFYING",
        _ => "RUNNING",
    }
}

/// Draw the banner. With no verdict yet, show the phase, so the footer is never
/// blank and never claims more than it knows.
pub fn show(ui: &mut Ui, verdict: Option<&VerdictReport>, phase: Phase, elapsed: f64) {
    let (accent, mark, headline, detail, guidance, reasons) = match verdict {
        Some(v) => (
            colour(v.state),
            glyph(v.state),
            v.headline().to_string(),
            v.detail(),
            Some(v.state.guidance()),
            v.reasons.clone(),
        ),
        None => (
            theme::DIMMER,
            phase_glyph(phase),
            phase_words(phase).to_string(),
            match phase {
                Phase::Idle => {
                    "No session running. Choose the cards and destinations, then Offload.".into()
                }
                other => format!("{}  ·  {elapsed:.0}s elapsed", other.label()),
            },
            None,
            Vec::new(),
        ),
    };

    // A faint wash of the state colour across the whole band, as the mockup
    // does. Subtle enough to read under, unmistakable at a glance.
    let wash = if verdict.is_some() {
        blend(theme::PANEL, accent, 0.07)
    } else {
        theme::PANEL
    };

    egui::Frame::default()
        .fill(wash)
        .stroke(Stroke::new(1.0, blend(theme::LINE, accent, 0.35)))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(egui::Margin::symmetric(16, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.with_layout(Layout::top_down(Align::LEFT), |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(mark)
                            .color(accent)
                            .font(theme::mono(theme::VERDICT * 0.8)),
                    );
                    ui.label(
                        RichText::new(spaced(&headline))
                            .color(accent)
                            .font(theme::mono(theme::VERDICT)),
                    );
                });
                ui.add_space(4.0);
                ui.label(
                    RichText::new(&detail)
                        .color(theme::DIM)
                        .font(theme::mono(theme::LOG)),
                );
                // What to do about it, in the state's own colour. The counts
                // above say what happened; this says what happens next, which is
                // the only thing a tired person actually needs from a banner.
                if let Some(g) = guidance {
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(g)
                            .color(accent)
                            .font(theme::mono(theme::SMALL)),
                    );
                }
                // The reasoning, never a bare verdict. Bounded so a run with
                // hundreds of failures cannot push the headline off screen --
                // the log pane above holds the complete list.
                for reason in reasons.iter().take(2) {
                    ui.label(
                        RichText::new(format!("· {reason}"))
                            .color(theme::DIMMER)
                            .font(theme::mono(theme::SMALL)),
                    );
                }
                if reasons.len() > 2 {
                    ui.label(
                        RichText::new(format!(
                            "· and {} more — filter the log to ERR",
                            reasons.len() - 2
                        ))
                        .color(theme::DIMMER)
                        .font(theme::mono(theme::SMALL)),
                    );
                }
            });
        });
}

/// Uppercase with hair spaces, standing in for CSS letter-spacing.
fn spaced(text: &str) -> String {
    text.chars().flat_map(|c| [c, '\u{2009}']).collect()
}

fn blend(base: Color32, tint: Color32, amount: f32) -> Color32 {
    let mix = |a: u8, b: u8| (a as f32 * (1.0 - amount) + b as f32 * amount) as u8;
    Color32::from_rgb(
        mix(base.r(), tint.r()),
        mix(base.g(), tint.g()),
        mix(base.b(), tint.b()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The words are the channel that always works: no colour, no glyph, no
    /// screenshot and no photocopy can blur them together.
    #[test]
    fn every_state_gets_its_own_words() {
        let states = Verdict::ALL;
        for i in 0..states.len() {
            for j in (i + 1)..states.len() {
                assert_ne!(
                    states[i].headline(),
                    states[j].headline(),
                    "{:?} and {:?} would read identically",
                    states[i],
                    states[j]
                );
                assert_ne!(states[i].guidance(), states[j].guidance());
            }
        }
    }

    /// The one state that authorises destroying the originals must never look
    /// like any of the four that do not -- not in hue, not in mark.
    #[test]
    fn the_permission_never_looks_like_a_refusal() {
        for state in Verdict::ALL {
            if state.authorises_erase() {
                continue;
            }
            assert_ne!(colour(Verdict::SafeToFormat), colour(state), "{state:?}");
            assert_ne!(glyph(Verdict::SafeToFormat), glyph(state), "{state:?}");
        }
    }

    /// The two tiers added for legibility exist precisely so that "you did
    /// everything your hardware allows" does not wear the same amber as "this
    /// drive is dropping writes". If they ever converge, the split has bought
    /// nothing.
    #[test]
    fn structural_limits_do_not_wear_the_colour_of_a_fault() {
        for ok in [Verdict::VerifiedOneSource, Verdict::VerifiedOneCopy] {
            assert!(!ok.something_is_wrong());
            for bad in [Verdict::VerifiedDoNotFormat, Verdict::Failed] {
                assert_ne!(colour(ok), colour(bad), "{ok:?} vs {bad:?}");
                assert_ne!(glyph(ok), glyph(bad), "{ok:?} vs {bad:?}");
            }
        }
    }

    /// The safety property, stated over every state that exists rather than over
    /// the three that existed when it was written.
    #[test]
    fn exactly_one_state_authorises_an_erase() {
        let yes: Vec<_> = Verdict::ALL
            .into_iter()
            .filter(|v| v.authorises_erase())
            .collect();
        assert_eq!(yes, vec![Verdict::SafeToFormat], "got {yes:?}");
    }

    #[test]
    fn only_a_failure_means_the_copy_is_bad() {
        for v in Verdict::ALL {
            assert_eq!(v.copy_is_good(), v != Verdict::Failed, "{v:?}");
        }
    }

    #[test]
    fn the_wash_stays_readable() {
        // A 7% tint must move the panel colour without swamping it, or text
        // sitting on top stops being legible.
        let washed = blend(theme::PANEL, theme::OK, 0.07);
        assert_ne!(washed, theme::PANEL);
        let delta = (washed.g() as i32 - theme::PANEL.g() as i32).abs();
        assert!((1..=40).contains(&delta), "tint moved green by {delta}");
    }

    #[test]
    fn running_phases_are_distinguishable_before_a_verdict_exists() {
        assert_ne!(phase_glyph(Phase::Idle), phase_glyph(Phase::Verify));
        assert_ne!(phase_words(Phase::Idle), phase_words(Phase::Copy));
        assert_eq!(phase_words(Phase::Verify), "VERIFYING");
    }
}
