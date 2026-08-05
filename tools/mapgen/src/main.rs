use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use worldgen::{MapPreset, generate_for_players, generate_preset_for_players, validate};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PresetArg {
    Dev,
    Playtest,
    Validation,
}

impl From<PresetArg> for MapPreset {
    fn from(value: PresetArg) -> Self {
        match value {
            PresetArg::Dev => Self::Dev,
            PresetArg::Playtest => Self::Playtest,
            PresetArg::Validation => Self::Validation,
        }
    }
}

#[derive(Debug, Parser)]
#[command(about = "Generate and validate a deterministic stepped-island map")]
struct Arguments {
    #[arg(long, value_enum, default_value_t = PresetArg::Dev)]
    preset: PresetArg,

    #[arg(long)]
    seed: Option<u64>,

    /// Number of players to seed (2 through 500).
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(2..=500))]
    players: u16,

    #[arg(long)]
    output: Option<PathBuf>,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let preset = MapPreset::from(arguments.preset);
    let generated = if let Some(seed) = arguments.seed {
        let (width, height) = preset.dimensions();
        generate_for_players(
            format!("{}-{seed}", preset.name()),
            width,
            height,
            seed,
            arguments.players,
        )
    } else {
        generate_preset_for_players(preset, arguments.players)
    };
    let report = validate(&generated).map_err(anyhow::Error::msg)?;

    if let Some(output) = arguments.output {
        if let Some(parent) = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let json = serde_json::to_vec_pretty(&generated).context("failed to serialize map")?;
        fs::write(&output, json)
            .with_context(|| format!("failed to write {}", output.display()))?;
        println!("Wrote {}", output.display());
    }

    println!(
        "{}: {}x{}, {} players, {} capturable / {} ground, {} slopes, {} cliffs, hash {:016x}",
        generated.manifest.name,
        generated.manifest.width,
        generated.manifest.height,
        generated.manifest.player_count,
        report.capturable_cells,
        report.ground_cells,
        report.slopes,
        report.cliffs,
        generated.manifest.content_hash,
    );
    Ok(())
}
