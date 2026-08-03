//! Pure, deterministic rules for the V1 hex-based RTS simulation.
//!
//! This crate deliberately has no engine, networking, database, wall-clock, or
//! random-number dependencies. Authoritative callers provide ordered commands
//! and logical steps; the functions here return reproducible integer results.

#![forbid(unsafe_code)]

pub mod combat;
pub mod connectivity;
pub mod conquest;
pub mod coord;
pub mod front;
pub mod map;
pub mod movement;
pub mod pathfinding;
pub mod redistribution;

pub use combat::{
    AttackFront, AttackOutcome, CombatConfig, CombatError, CombatResolution, resolve_edge_combat,
};
pub use connectivity::{connected_components, owned_components};
pub use conquest::{ConquestError, ConquestProgress, ConquestRule};
pub use coord::{Axial, ChunkAddress, ChunkCoord, Cube, HexDirection, HexEdge};
pub use front::{DirectedFrontEdge, FrontSelectionError, selected_front_edges};
pub use map::{
    Cell, EdgeLimits, ForceComposition, HexMap, LogisticsConfig, MovementConfig, PlayerId,
    Strength, TerrainKind, Traversal, ground_traversal,
};
pub use movement::{
    MovementError, MovementIntent, MovementLimit, MovementOutcome, MovementStep, PlanError,
    TransferLeg, TransferPlan, TransferRequest, movement_step, plan_transfer,
};
pub use pathfinding::{Path, shortest_path};
pub use redistribution::{
    DistributionError, DistributionPreset, TargetDistribution, distribution_weights,
    redistribution_targets,
};
