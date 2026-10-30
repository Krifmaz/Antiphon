//! The microphone chain as Voice's "Fine-tune" tab shows it: every stage
//! and EQ band of a [`ChainConfig`] turned into [`ui::StageRow`]s and
//! [`ui::BandRow`]s from the one table in [`controls`], and the tab's
//! actions applied back to the chain.
//!
//! Shared word for word by the desktop and the phone app (which compiles
//! this file in by `#[path]`), so both show the same controls and change
//! the chain the same way.

use antiphon_audio::{controls, BandType, ChainConfig};
use antiphon_ui as ui;

/// Every stage of `chain` with its settings, in the order the sound runs
/// through them. `reduction` gives, for a stage id, how far that stage is
/// turning the voice down right now (`None` outside a call, or for a
/// stage with nothing to show).
pub fn stages(chain: &ChainConfig, reduction: impl Fn(&str) -> Option<f32>) -> Vec<ui::StageRow> {
    controls::STAGES
        .iter()
        .map(|stage| {
            let enabled = stage.is_on(chain);
            ui::StageRow {
                id: stage.id,
                name: stage.name,
                help: stage.help,
                enabled,
                params: stage.params(chain).iter().map(|p| row(p, p.value(chain))).collect(),
                // Only the gate and the compressor turn the voice down by
                // a varying amount worth watching, and only while on.
                reduction_db: if enabled && matches!(stage.id, "noise_gate" | "compressor") { reduction(stage.id) } else { None },
            }
        })
        .collect()
}

/// The EQ bands of `chain`, each with its settings.
pub fn bands(chain: &ChainConfig) -> Vec<ui::BandRow> {
    chain
        .eq_bands
        .iter()
        .map(|band| ui::BandRow {
            shape: match band.band_type {
                BandType::Bell => "Bell",
                BandType::LowShelf => "Low shelf",
                BandType::HighShelf => "High shelf",
                BandType::Notch => "Notch",
            },
            enabled: band.enabled,
            params: controls::BAND_PARAMS.iter().map(|p| row(p, p.value(band))).collect(),
        })
        .collect()
}

/// Applies one of the tab's actions to `chain`. `true` when the chain
/// changed and must be saved and handed to a running call; `false` for an
/// action that is not the tab's, or ids that do not exist.
pub fn apply(chain: &mut ChainConfig, action: &ui::Action) -> bool {
    match *action {
        ui::Action::StageOn(stage, on) => controls::set_stage(chain, stage, on),
        ui::Action::StageParam(stage, param, value) => controls::set_param(chain, stage, param, value),
        ui::Action::BandParam(band, param, value) => controls::set_band(chain, band, param, value),
        _ => false,
    }
}

/// One setting as the screen shows it, at `value`.
fn row<T>(p: &controls::Param<T>, value: f32) -> ui::ParamRow {
    ui::ParamRow { id: p.id, name: p.name, help: p.help, unit: p.unit, min: p.min, max: p.max, step: p.step, value }
}
