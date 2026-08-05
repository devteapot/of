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

    /// Preferred player slot, 1 through 500 (env: `OF_PLAYER`).
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..=500))]
    player: Option<u16>,

    /// Display name used by `join_match` (env: `OF_NAME`).
    #[arg(long)]
    name: Option<String>,

    /// Credential profile used for the persisted auth token (env: `OF_PROFILE`).
    #[arg(long)]
    profile: Option<String>,

    /// Start with persistent cluster-policy packet animations visible.
    ///
    /// This presentation-only switch is intentionally absent from release
    /// builds so it cannot become part of the normal player control surface.
    #[cfg(debug_assertions)]
    #[arg(long)]
    debug_policy_flows: bool,
}

#[derive(Resource, Clone, Debug)]
pub struct ClientConfig {
    pub offline: bool,
    pub host: String,
    pub database: String,
    pub preferred_player: u16,
    pub display_name: String,
    pub profile: String,
    /// Presentation-only diagnostic state. Debug clients may toggle it at
    /// runtime with F4; authoritative policy execution and troop accounting
    /// never consult this value.
    pub debug_policy_flows: bool,
}

impl ClientConfig {
    pub fn from_process() -> Self {
        let args = ClientArgs::parse();
        let debug_policy_flows = debug_policy_flows(&args);
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
            debug_policy_flows,
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

#[cfg(debug_assertions)]
const fn debug_policy_flows(args: &ClientArgs) -> bool {
    args.debug_policy_flows
}

#[cfg(not(debug_assertions))]
const fn debug_policy_flows(_args: &ClientArgs) -> bool {
    false
}

fn env_nonempty(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn env_u16(key: &str) -> Option<u16> {
    env_nonempty(key)?.parse().ok()
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

    #[cfg(debug_assertions)]
    #[test]
    fn debug_build_keeps_policy_flows_off_unless_requested() {
        let normal = ClientArgs::try_parse_from(["game-client"])
            .expect("debug clients accept their normal command line");
        assert!(!debug_policy_flows(&normal));

        let args = ClientArgs::try_parse_from(["game-client", "--debug-policy-flows"])
            .expect("debug builds expose the diagnostic flag");
        assert!(debug_policy_flows(&args));
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn release_build_rejects_the_policy_flow_diagnostic_flag() {
        assert!(
            ClientArgs::try_parse_from(["game-client", "--debug-policy-flows"]).is_err(),
            "release CLI must not expose the debug-only flag"
        );
    }
}
