use std::{env, path::PathBuf};

use bevy::prelude::Resource;
use clap::Parser;

const DEFAULT_HOST: &str = "http://127.0.0.1:3000";
const DEFAULT_DATABASE: &str = "of-match-dev";

#[derive(Parser, Debug)]
#[command(
    name = "game-client",
    about = "Native V1 hex RTS client",
    disable_version_flag = true
)]
struct ClientArgs {
    /// Use the local deterministic fixture instead of `SpacetimeDB`.
    #[arg(long)]
    offline: bool,

    /// `SpacetimeDB` host URI (env: `OF_HOST`).
    #[arg(long)]
    host: Option<String>,

    /// `SpacetimeDB` database name or identity (env: `OF_DATABASE`).
    #[arg(long)]
    database: Option<String>,

    /// Preferred player slot, 1 or 2 (env: `OF_PLAYER`).
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=2))]
    player: Option<u8>,

    /// Display name used by `join_match` (env: `OF_NAME`).
    #[arg(long)]
    name: Option<String>,

    /// Credential profile used for the persisted auth token (env: `OF_PROFILE`).
    #[arg(long)]
    profile: Option<String>,
}

#[derive(Resource, Clone, Debug)]
pub struct ClientConfig {
    pub offline: bool,
    pub host: String,
    pub database: String,
    pub preferred_player: u8,
    pub display_name: String,
    pub profile: String,
}

impl ClientConfig {
    pub fn from_process() -> Self {
        let args = ClientArgs::parse();
        let preferred_player = args
            .player
            .or_else(|| env_u8("OF_PLAYER"))
            .filter(|player| matches!(player, 1 | 2))
            .unwrap_or(1);
        let profile = args
            .profile
            .or_else(|| env_nonempty("OF_PROFILE"))
            .unwrap_or_else(|| format!("player-{preferred_player}"));
        let profile = safe_profile(&profile).unwrap_or_else(|| {
            eprintln!("invalid profile {profile:?}: use only ASCII letters, digits, '-' and '_'");
            std::process::exit(2);
        });
        Self {
            offline: args.offline || env_flag("OF_OFFLINE"),
            host: args
                .host
                .or_else(|| env_nonempty("OF_HOST"))
                .or_else(|| env_nonempty("SPACETIMEDB_HOST"))
                .unwrap_or_else(|| DEFAULT_HOST.to_owned()),
            database: args
                .database
                .or_else(|| env_nonempty("OF_DATABASE"))
                .or_else(|| env_nonempty("SPACETIMEDB_DATABASE"))
                .unwrap_or_else(|| DEFAULT_DATABASE.to_owned()),
            display_name: args
                .name
                .or_else(|| env_nonempty("OF_NAME"))
                .unwrap_or_else(|| format!("Player {preferred_player}")),
            preferred_player,
            profile,
        }
    }

    pub fn token_path(&self) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(".spacetime-data")
            .join(format!("client-{}.token", self.profile))
    }

    pub const fn mode_label(&self) -> &'static str {
        if self.offline { "Offline" } else { "Online" }
    }
}

fn env_nonempty(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn env_u8(name: &str) -> Option<u8> {
    env_nonempty(name).and_then(|value| value.parse().ok())
}

fn env_flag(name: &str) -> bool {
    env_nonempty(name).is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

fn safe_profile(profile: &str) -> Option<String> {
    let trimmed = profile.trim();
    (!trimmed.is_empty()
        && trimmed
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_')))
    .then(|| trimmed.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_is_path_safe() {
        assert_eq!(safe_profile("player_2"), Some("player_2".to_owned()));
        assert_eq!(safe_profile("../../token"), None);
        assert_eq!(safe_profile(""), None);
    }
}
