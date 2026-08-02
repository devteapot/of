use std::collections::BTreeSet;

use bevy::{
    asset::RenderAssetUsages,
    color::{ColorToComponents, LinearRgba},
    mesh::Indices,
    prelude::*,
    render::render_resource::PrimitiveTopology,
};
use hex_core::{Axial, ChunkCoord, TerrainKind};

use crate::{
    geometry::{COLUMN_FLOOR, cell_top, chunk_of, corner, world_center},
    model::{CellView, MatchView, PLAYER_ONE, PLAYER_TWO},
};

#[derive(Component, Debug)]
pub struct TerrainChunk {
    pub coordinate: ChunkCoord,
    pub triangle_to_cell: Vec<Axial>,
}

#[derive(Resource)]
pub struct TerrainMaterial(Handle<StandardMaterial>);

#[derive(Default)]
struct ChunkMeshBuilder {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    colors: Vec<[f32; 4]>,
    indices: Vec<u32>,
    triangle_to_cell: Vec<Axial>,
}

impl ChunkMeshBuilder {
    fn vertex(&mut self, position: Vec3, normal: Vec3, color: [f32; 4]) -> u32 {
        let index = self.positions.len() as u32;
        self.positions.push(position.to_array());
        self.normals.push(normal.to_array());
        self.colors.push(color);
        index
    }

    fn triangle(&mut self, cell: Axial, a: u32, b: u32, c: u32) {
        self.indices.extend_from_slice(&[a, b, c]);
        self.triangle_to_cell.push(cell);
    }

    fn finish(self) -> (Mesh, Vec<Axial>) {
        let mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::MAIN_WORLD | RenderAssetUsages::RENDER_WORLD,
        )
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, self.positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, self.normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, self.colors)
        .with_inserted_indices(Indices::U32(self.indices));
        (mesh, self.triangle_to_cell)
    }
}

pub fn spawn_terrain(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut view: ResMut<MatchView>,
) {
    let material = materials.add(StandardMaterial {
        base_color: Color::WHITE,
        perceptual_roughness: 0.96,
        metallic: 0.0,
        ..default()
    });
    commands.insert_resource(TerrainMaterial(material.clone()));
    let chunks: BTreeSet<_> = view.cells.keys().copied().map(chunk_of).collect();
    for chunk in chunks {
        let (mesh, triangle_to_cell) = build_chunk_mesh(&view, chunk);
        commands.spawn((
            Name::new(format!("Terrain chunk {},{}", chunk.q, chunk.r)),
            TerrainChunk {
                coordinate: chunk,
                triangle_to_cell,
            },
            Mesh3d(meshes.add(mesh)),
            MeshMaterial3d(material.clone()),
        ));
    }
    view.dirty_chunks.clear();
}

pub fn sync_terrain_chunks(
    mut commands: Commands,
    mut view: ResMut<MatchView>,
    mut chunks: Query<(Entity, &mut TerrainChunk, &Mesh3d)>,
    mut meshes: ResMut<Assets<Mesh>>,
    material: Res<TerrainMaterial>,
) {
    let dirty = std::mem::take(&mut view.dirty_chunks);
    if dirty.is_empty() {
        return;
    }
    let desired: BTreeSet<_> = view.cells.keys().copied().map(chunk_of).collect();
    let existing: BTreeSet<_> = chunks
        .iter()
        .map(|(_, chunk, _)| chunk.coordinate)
        .collect();
    for (entity, chunk, _) in &chunks {
        if !desired.contains(&chunk.coordinate) {
            commands.entity(entity).despawn();
        }
    }
    for coordinate in desired.difference(&existing) {
        let (mesh, triangle_to_cell) = build_chunk_mesh(&view, *coordinate);
        commands.spawn((
            Name::new(format!("Terrain chunk {},{}", coordinate.q, coordinate.r)),
            TerrainChunk {
                coordinate: *coordinate,
                triangle_to_cell,
            },
            Mesh3d(meshes.add(mesh)),
            MeshMaterial3d(material.0.clone()),
        ));
    }

    for (_, mut chunk, mesh_handle) in &mut chunks {
        if !dirty.contains(&chunk.coordinate) {
            continue;
        }
        let (replacement, triangle_to_cell) = build_chunk_mesh(&view, chunk.coordinate);
        if let Some(mut mesh) = meshes.get_mut(mesh_handle) {
            *mesh = replacement;
        }
        chunk.triangle_to_cell = triangle_to_cell;
    }
}

fn build_chunk_mesh(view: &MatchView, chunk: ChunkCoord) -> (Mesh, Vec<Axial>) {
    let mut builder = ChunkMeshBuilder::default();
    for cell in view
        .cells
        .values()
        .filter(|cell| chunk_of(cell.coordinate) == chunk)
    {
        push_cell(&mut builder, view, cell);
    }
    builder.finish()
}

fn push_cell(builder: &mut ChunkMeshBuilder, view: &MatchView, cell: &CellView) {
    let top_y = cell_top(cell.elevation, cell.is_water());
    let center = world_center(cell.coordinate, cell.elevation, cell.is_water());
    let top_color = cell_color(cell);

    let center_index = builder.vertex(center, Vec3::Y, top_color);
    let top_corners = std::array::from_fn::<_, 6, _>(|index| {
        builder.vertex(corner(center, index, top_y), Vec3::Y, top_color)
    });
    for index in 0..6 {
        let next = (index + 1) % 6;
        // Clockwise winding in XZ points the normal toward +Y.
        builder.triangle(
            cell.coordinate,
            center_index,
            top_corners[next],
            top_corners[index],
        );
    }

    for (edge, neighbor_coord) in cell.coordinate.neighbors().into_iter().enumerate() {
        let neighbor_top = view.cell(neighbor_coord).map_or(COLUMN_FLOOR, |neighbor| {
            cell_top(neighbor.elevation, neighbor.is_water())
        });
        let bottom_y = if neighbor_top + 0.015 < top_y {
            neighbor_top.max(COLUMN_FLOOR)
        } else {
            (top_y - 0.065).max(COLUMN_FLOOR)
        };
        if bottom_y >= top_y {
            continue;
        }

        let next = (edge + 1) % 6;
        let top_a = corner(center, edge, top_y);
        let top_b = corner(center, next, top_y);
        let bottom_a = corner(center, edge, bottom_y);
        let bottom_b = corner(center, next, bottom_y);
        let normal = Vec3::new(
            (top_a.x + top_b.x) * 0.5 - center.x,
            0.0,
            (top_a.z + top_b.z) * 0.5 - center.z,
        )
        .normalize();
        let depth = ((top_y - bottom_y) / 2.4).clamp(0.0, 1.0);
        let side_color = shade(top_color, 0.52 - depth * 0.12);
        let a = builder.vertex(bottom_a, normal, side_color);
        let b = builder.vertex(bottom_b, normal, side_color);
        let c = builder.vertex(top_b, normal, shade(top_color, 0.68));
        let d = builder.vertex(top_a, normal, shade(top_color, 0.68));
        builder.triangle(cell.coordinate, a, c, b);
        builder.triangle(cell.coordinate, a, d, c);
    }
}

fn cell_color(cell: &CellView) -> [f32; 4] {
    let base = if cell.is_water() {
        Color::srgb(0.055, 0.16, 0.21)
    } else {
        match cell.owner {
            Some(PLAYER_ONE) => Color::srgb(0.06, 0.48, 0.58),
            Some(PLAYER_TWO) => Color::srgb(0.76, 0.24, 0.16),
            _ => match cell.terrain {
                TerrainKind::Plains => Color::srgb(0.39, 0.43, 0.30),
                TerrainKind::Hills => Color::srgb(0.42, 0.38, 0.25),
                TerrainKind::Mountain => Color::srgb(0.36, 0.35, 0.31),
                TerrainKind::Water => Color::srgb(0.055, 0.16, 0.21),
            },
        }
    };
    let mut linear = LinearRgba::from(base);
    let density = cell.density();
    let terrain_light = match cell.terrain {
        TerrainKind::Plains => 1.0,
        TerrainKind::Hills => 0.92,
        TerrainKind::Mountain => 0.80,
        TerrainKind::Water => 0.82,
    };
    let ownership_readability = 0.72 + density * 0.48;
    linear.red = (linear.red * ownership_readability * terrain_light + density * 0.035).min(1.0);
    linear.green =
        (linear.green * ownership_readability * terrain_light + density * 0.045).min(1.0);
    linear.blue = (linear.blue * ownership_readability * terrain_light + density * 0.05).min(1.0);
    linear.to_f32_array()
}

fn shade(color: [f32; 4], factor: f32) -> [f32; 4] {
    [
        color[0] * factor,
        color[1] * factor,
        color[2] * factor,
        color[3],
    ]
}
