#[cfg(not(target_arch = "wasm32"))]
use std::{env, path::PathBuf};

use bevy::prelude::Resource;
#[cfg(not(target_arch = "wasm32"))]
use clap::Parser;

#[cfg(not(target_arch = "wasm32"))]
const DEFAULT_HOST: &str = "http://127.0.0.1:3000";
#[cfg(target_arch = "wasm32")]
const DEFAULT_HOST: &str = match option_env!("OF_WEB_HOST") {
    Some(host) => host,
    None => "http://127.0.0.1:3000",
};

#[cfg(not(target_arch = "wasm32"))]
const DEFAULT_DATABASE: &str = "of-match-dev";
#[cfg(target_arch = "wasm32")]
const DEFAULT_DATABASE: &str = match option_env!("OF_WEB_DATABASE") {
    Some(database) => database,
    None => "of-match-dev",
};

#[cfg(not(target_arch = "wasm32"))]
#[derive(Parser, Debug)]
#[command(
    name = "game-client",
    about = "Native hex RTS client",
    disable_version_flag = true
)]
struct ClientArgs {
    /// Use the local deterministic fixture instead of `SpacetimeDB`.
    #[arg(long)]
    offline: bool,

    /// Generate a composable layered V2 map for the offline viewer.
    #[arg(long, requires = "offline")]
    worldgen_v2: bool,

    /// Width of the generated V2 viewer map (default: 256).
    #[arg(
        long,
        requires = "worldgen_v2",
        value_parser = clap::value_parser!(u32).range(24..)
    )]
    map_width: Option<u32>,

    /// Height of the generated V2 viewer map (default: 256).
    #[arg(
        long,
        requires = "worldgen_v2",
        value_parser = clap::value_parser!(u32).range(24..)
    )]
    map_height: Option<u32>,

    /// Seed for the generated V2 viewer map (default: 42).
    #[arg(long, requires = "worldgen_v2")]
    map_seed: Option<u64>,

    /// Player spawn regions generated on the V2 viewer map (default: 2).
    #[arg(
        long,
        requires = "worldgen_v2",
        value_parser = clap::value_parser!(u16).range(2..=500)
    )]
    map_players: Option<u16>,

    /// `SpacetimeDB` host URI (env: `OF_HOST`).
    #[arg(long)]
    host: Option<String>,

    /// `SpacetimeDB` database name or identity (env: `OF_DATABASE`).
    #[arg(long)]
    database: Option<String>,

    /// Preferred player slot, 1 through 500 (env: `OF_PLAYER`).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=500))]
    player: Option<u16>,

    /// Display name used by `join_match` (env: `OF_NAME`).
    #[arg(long)]
    name: Option<String>,

    /// Credential profile used for the persisted auth token (env: `OF_PROFILE`).
    #[arg(long)]
    profile: Option<String>,

    /// Skip the lobby UI and join immediately (env: `OF_AUTO_JOIN`).
    #[arg(long)]
    auto_join: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayeredWorldOptions {
    pub width: u32,
    pub height: u32,
    pub seed: u64,
    pub players: u16,
}

#[derive(Resource, Clone, Debug)]
pub struct ClientConfig {
    pub offline: bool,
    pub layered_world: Option<LayeredWorldOptions>,
    pub host: String,
    pub database: String,
    pub preferred_player: u16,
    pub display_name: String,
    pub profile: String,
    /// Automation/dev path: connect and call `join_match` after bootstrap.
    pub auto_join: bool,
}

impl ClientConfig {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_process() -> Self {
        let args = ClientArgs::parse();
        Self::from_args(args)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn from_args(args: ClientArgs) -> Self {
        let explicit_player = args.player.is_some() || env_nonempty("OF_PLAYER").is_some();
        let auto_join = args.auto_join || env_flag("OF_AUTO_JOIN") || explicit_player;
        let preferred_player = args
            .player
            .or_else(|| env_u16("OF_PLAYER"))
            .filter(|player| (1..=500).contains(player))
            .unwrap_or(1);
        let profile = args
            .profile
            .or_else(|| env_nonempty("OF_PROFILE"))
            .unwrap_or_else(|| format!("player-{preferred_player}"));
        let profile = safe_profile(&profile).unwrap_or_else(|| {
            eprintln!("invalid profile {profile:?}: use only ASCII letters, digits, '-' and '_'");
            std::process::exit(2);
        });
        let display_name = args
            .name
            .or_else(|| env_nonempty("OF_NAME"))
            .unwrap_or_else(|| {
                if auto_join {
                    format!("Player {preferred_player}")
                } else {
                    String::new()
                }
            });
        Self {
            offline: args.offline || env_flag("OF_OFFLINE"),
            layered_world: args.worldgen_v2.then(|| LayeredWorldOptions {
                width: args.map_width.unwrap_or(256),
                height: args.map_height.unwrap_or(256),
                seed: args.map_seed.unwrap_or(42),
                players: args.map_players.unwrap_or(2),
            }),
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
            display_name,
            preferred_player,
            profile,
            auto_join,
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn from_process() -> Self {
        let explicit_player = browser_param("player").is_some();
        let auto_join = browser_flag("autojoin") || explicit_player;
        let preferred_player = browser_param("player")
            .and_then(|value| value.parse().ok())
            .filter(|player| (1..=500).contains(player))
            .unwrap_or(1);
        let profile =
            browser_param("profile").unwrap_or_else(|| format!("player-{preferred_player}"));
        let profile =
            safe_profile(&profile).unwrap_or_else(|| format!("player-{preferred_player}"));

        Self {
            offline: browser_flag("offline"),
            layered_world: None,
            host: browser_param("host").unwrap_or_else(|| DEFAULT_HOST.to_owned()),
            database: browser_param("database")
                .or_else(|| browser_param("db"))
                .unwrap_or_else(|| DEFAULT_DATABASE.to_owned()),
            display_name: browser_param("name").unwrap_or_else(|| {
                if auto_join {
                    format!("Player {preferred_player}")
                } else {
                    String::new()
                }
            }),
            preferred_player,
            profile,
            auto_join,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn token_path(&self) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(".spacetime-data")
            .join(format!("client-{}.token", self.profile))
    }

    #[cfg(target_arch = "wasm32")]
    pub fn token_storage_key(&self) -> String {
        format!("of.auth.{}.{}.{}", self.host, self.database, self.profile)
    }

    pub const fn mode_label(&self) -> &'static str {
        if self.layered_world.is_some() {
            "Offline V2"
        } else if self.offline {
            "Offline"
        } else {
            "Online"
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn env_nonempty(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(not(target_arch = "wasm32"))]
fn env_u16(key: &str) -> Option<u16> {
    env_nonempty(key)?.parse().ok()
}

#[cfg(not(target_arch = "wasm32"))]
fn env_flag(name: &str) -> bool {
    env_nonempty(name).is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(target_arch = "wasm32")]
fn browser_param(name: &str) -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    let params = web_sys::UrlSearchParams::new_with_str(&search).ok()?;
    params
        .get(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(target_arch = "wasm32")]
fn browser_flag(name: &str) -> bool {
    browser_param(name).is_some_and(|value| {
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

    #[test]
    fn layered_viewer_arguments_have_safe_defaults() {
        let args = ClientArgs::try_parse_from(["game-client", "--offline", "--worldgen-v2"])
            .expect("layered viewer arguments");
        let config = ClientConfig::from_args(args);
        assert_eq!(
            config.layered_world,
            Some(LayeredWorldOptions {
                width: 256,
                height: 256,
                seed: 42,
                players: 2,
            })
        );
        assert_eq!(config.mode_label(), "Offline V2");
    }

    #[test]
    fn layered_viewer_requires_offline_mode() {
        let error = ClientArgs::try_parse_from(["game-client", "--worldgen-v2"])
            .expect_err("online layered generation must be rejected");
        assert!(error.to_string().contains("--offline"));
    }
}
