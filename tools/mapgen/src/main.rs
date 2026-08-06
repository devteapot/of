use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use worldgen::v2::{
    Biome, Landform, LayeredWorld, Surface, WorldPipeline, WorldSpec, validate as validate_v2,
};
use worldgen::{MapPreset, generate_for_players, generate_preset_for_players, validate};

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum GeneratorArg {
    #[default]
    V1,
    V2,
}

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

#[derive(Clone, Copy, Debug, ValueEnum)]
enum LayerArg {
    Elevation,
    Surface,
    Landform,
    Biome,
    Moisture,
    Fertility,
    Rivers,
    Gameplay,
}

#[derive(Debug, Parser)]
#[command(about = "Generate, inspect, validate, and chunk deterministic maps")]
struct Arguments {
    /// Preserve V1 output or use the composable layered V2 pipeline.
    #[arg(long, value_enum, default_value_t)]
    generator: GeneratorArg,

    #[arg(long, value_enum, default_value_t = PresetArg::Dev)]
    preset: PresetArg,

    #[arg(long)]
    seed: Option<u64>,

    /// Number of players to seed (2 through 500).
    #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u16).range(2..=500))]
    players: u16,

    /// Custom V2 width. Must be paired with `--height`.
    #[arg(long)]
    width: Option<u32>,

    /// Custom V2 height. Must be paired with `--width`.
    #[arg(long)]
    height: Option<u32>,

    #[arg(long, default_value_t = 64)]
    chunk_size: u16,

    #[arg(long, default_value_t = 32)]
    macro_cell_size: u16,

    /// Approximate mountain-range coverage, in basis points.
    #[arg(long, default_value_t = 3_000, value_parser = clap::value_parser!(u16).range(0..=10_000))]
    mountain_density_bps: u16,

    /// Minimum filled-depression depth that becomes a lake.
    #[arg(long, default_value_t = 18)]
    lake_depth_threshold: i16,

    /// Upstream accumulation required for a river; zero selects an area-scaled default.
    #[arg(long, default_value_t = 0)]
    river_threshold: u32,

    /// Complete generated map as inspectable JSON.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Export every V2 terrain chunk and its manifest into this directory.
    #[arg(long)]
    chunks_dir: Option<PathBuf>,

    /// Render one V2 layer as a portable graymap image.
    #[arg(long, value_enum)]
    inspect_layer: Option<LayerArg>,

    #[arg(long, requires = "inspect_layer")]
    inspect_output: Option<PathBuf>,
}

fn main() -> Result<()> {
    let arguments = Arguments::parse();
    match arguments.generator {
        GeneratorArg::V1 => run_v1(&arguments),
        GeneratorArg::V2 => run_v2(&arguments),
    }
}

fn run_v1(arguments: &Arguments) -> Result<()> {
    if arguments.width.is_some()
        || arguments.height.is_some()
        || arguments.chunks_dir.is_some()
        || arguments.inspect_layer.is_some()
    {
        bail!("custom dimensions, chunks, and layer inspection require --generator v2");
    }
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
    if let Some(output) = &arguments.output {
        write_pretty_json(output, &generated)?;
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

fn run_v2(arguments: &Arguments) -> Result<()> {
    let preset = MapPreset::from(arguments.preset);
    let preset_dimensions = preset.dimensions();
    let (width, height) = match (arguments.width, arguments.height) {
        (Some(width), Some(height)) => (width, height),
        (None, None) => (
            u32::from(preset_dimensions.0),
            u32::from(preset_dimensions.1),
        ),
        _ => bail!("--width and --height must be provided together"),
    };
    let seed = arguments.seed.unwrap_or_else(|| preset.seed());
    let mut spec = WorldSpec::new(format!("{}-layered-v2", preset.name()), width, height, seed);
    spec.player_count = arguments.players;
    spec.chunk_size = arguments.chunk_size;
    spec.macro_cell_size = arguments.macro_cell_size;
    spec.parameters.mountain_density_bps = arguments.mountain_density_bps;
    spec.parameters.lake_depth_threshold = arguments.lake_depth_threshold;
    spec.parameters.river_threshold = arguments.river_threshold;

    let (world, pass_reports) = WorldPipeline::default_v2()
        .run(&spec)
        .map_err(anyhow::Error::msg)?;
    let report = validate_v2(&world).map_err(anyhow::Error::msg)?;
    for pass in pass_reports {
        println!(
            "pass {:<12} cells {:>8} edges {:>6}{}",
            pass.name,
            pass.changed_cells,
            pass.changed_edges,
            pass.notes
                .first()
                .map_or_else(String::new, |note| format!("  {note}")),
        );
    }
    if let Some(output) = &arguments.output {
        write_pretty_json(output, &world)?;
    }
    if let Some(directory) = &arguments.chunks_dir {
        write_chunks(directory, &world)?;
    }
    if let Some(layer) = arguments.inspect_layer {
        let output = arguments
            .inspect_output
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("{}-{layer:?}.pgm", world.manifest.name)));
        write_layer_image(&output, &world, layer)?;
    }
    println!(
        "{}: {}x{}, {} players, {} land, {} lakes / {} cells, {} river cells, {} chunks, hash {:016x}",
        world.manifest.name,
        world.width(),
        world.height(),
        world.manifest.player_count,
        report.land_cells,
        report.water_bodies,
        report.lake_cells,
        report.river_cells,
        report.chunks,
        world.manifest.content_hash,
    );
    Ok(())
}

fn ensure_parent(path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    Ok(())
}

fn write_pretty_json(path: &std::path::Path, value: &impl serde::Serialize) -> Result<()> {
    ensure_parent(path)?;
    let json = serde_json::to_vec_pretty(value).context("failed to serialize generated map")?;
    fs::write(path, json).with_context(|| format!("failed to write {}", path.display()))?;
    println!("Wrote {}", path.display());
    Ok(())
}

fn write_chunks(directory: &std::path::Path, world: &LayeredWorld) -> Result<()> {
    fs::create_dir_all(directory)
        .with_context(|| format!("failed to create {}", directory.display()))?;
    write_pretty_json(&directory.join("manifest.json"), &world.manifest)?;
    for chunk_r in 0..world.chunks_high() {
        for chunk_q in 0..world.chunks_wide() {
            let coordinate = hex_core::ChunkCoord {
                q: i32::try_from(chunk_q).context("chunk q exceeds i32")?,
                r: i32::try_from(chunk_r).context("chunk r exceeds i32")?,
            };
            let chunk = world
                .chunk(coordinate)
                .with_context(|| format!("missing generated chunk {chunk_q},{chunk_r}"))?;
            write_pretty_json(
                &directory.join(format!("chunk-{chunk_q}-{chunk_r}.json")),
                &chunk,
            )?;
        }
    }
    Ok(())
}

fn write_layer_image(path: &std::path::Path, world: &LayeredWorld, layer: LayerArg) -> Result<()> {
    ensure_parent(path)?;
    let elevations = world.cells().iter().map(|cell| cell.elevation);
    let minimum = elevations.clone().min().unwrap_or_default();
    let maximum = elevations.max().unwrap_or(minimum);
    let range = i32::from(maximum).saturating_sub(i32::from(minimum)).max(1);
    let mut image = format!("P5\n{} {}\n255\n", world.width(), world.height()).into_bytes();
    image.extend(world.cells().iter().map(|cell| match layer {
        LayerArg::Elevation => {
            let normalized = (i32::from(cell.elevation) - i32::from(minimum)) * 255 / range;
            u8::try_from(normalized).unwrap_or_default()
        }
        LayerArg::Surface => match cell.surface {
            Surface::Ocean => 0,
            Surface::Lake => 75,
            Surface::Land => 205,
        },
        LayerArg::Landform => match cell.landform {
            Landform::Plain => 50,
            Landform::Valley => 90,
            Landform::Hill => 145,
            Landform::Plateau => 190,
            Landform::Mountain => 245,
        },
        LayerArg::Biome => match cell.biome {
            Biome::Dryland => 35,
            Biome::Tundra => 70,
            Biome::TemperateGrassland => 115,
            Biome::Forest => 160,
            Biome::Alpine => 205,
            Biome::Wetland => 245,
        },
        LayerArg::Moisture => cell.moisture,
        LayerArg::Fertility => cell.fertility,
        LayerArg::Rivers => {
            if cell.river.is_some() {
                255
            } else {
                0
            }
        }
        LayerArg::Gameplay => {
            if cell.gameplay.habitable {
                255
            } else if cell.gameplay.passable {
                130
            } else {
                0
            }
        }
    }));
    fs::write(path, image).with_context(|| format!("failed to write {}", path.display()))?;
    println!("Wrote {}", path.display());
    Ok(())
}
