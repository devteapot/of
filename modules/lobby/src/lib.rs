#![forbid(unsafe_code)]

use spacetimedb::{Identity, ReducerContext, SpacetimeType, Table};

const CONFIG_ID: u8 = 0;
const MIN_PLAYER_COUNT: u16 = 2;
const MAX_PLAYER_COUNT: u16 = 500;
const MAX_DISPLAY_NAME_CHARS: usize = 32;

#[derive(SpacetimeType, Clone, Copy, Debug, Eq, PartialEq)]
pub enum LobbyMapPreset {
    Small,
    Medium,
    Large,
}

#[derive(SpacetimeType, Clone, Copy, Debug, Eq, PartialEq)]
pub enum LobbyStatus {
    Pending,
    Provisioning,
    Open,
    Full,
    Failed,
    Cancelled,
}

#[derive(Clone)]
#[spacetimedb::table(accessor = control_config)]
pub struct ControlConfig {
    #[primary_key]
    pub singleton_id: u8,
    pub owner_identity: Identity,
}

#[derive(Clone)]
#[spacetimedb::table(
    accessor = lobby,
    public,
    index(accessor = lobby_by_creator, btree(columns = [creator_identity])),
    index(accessor = lobby_by_status, btree(columns = [status]))
)]
pub struct Lobby {
    #[primary_key]
    pub lobby_id: String,
    pub creator_identity: Identity,
    pub map_preset: LobbyMapPreset,
    pub player_count: u16,
    pub member_count: u16,
    pub status: LobbyStatus,
    pub match_database: String,
    pub failure_reason: String,
    pub created_at_us: u64,
    pub updated_at_us: u64,
}

#[derive(Clone)]
#[spacetimedb::table(
    accessor = lobby_member,
    public,
    index(accessor = member_by_lobby, btree(columns = [lobby_id])),
    index(accessor = member_by_identity, btree(columns = [identity]))
)]
pub struct LobbyMember {
    #[primary_key]
    pub member_key: String,
    pub lobby_id: String,
    pub identity: Identity,
    pub display_name: String,
    pub joined_at_us: u64,
}

fn timestamp_us(ctx: &ReducerContext) -> u64 {
    ctx.timestamp
        .to_duration_since_unix_epoch()
        .unwrap_or_default()
        .as_micros() as u64
}

fn clean_display_name(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("display name is required".to_owned());
    }
    Ok(value.chars().take(MAX_DISPLAY_NAME_CHARS).collect())
}

fn validate_lobby_id(lobby_id: &str) -> Result<(), String> {
    let valid = (8..=24).contains(&lobby_id.len())
        && lobby_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit());
    if valid {
        Ok(())
    } else {
        Err("lobby ID must contain 8-24 lowercase ASCII letters or digits".to_owned())
    }
}

fn validate_player_count(player_count: u16) -> Result<(), String> {
    if (MIN_PLAYER_COUNT..=MAX_PLAYER_COUNT).contains(&player_count) {
        Ok(())
    } else {
        Err(format!(
            "player count must be between {MIN_PLAYER_COUNT} and {MAX_PLAYER_COUNT}"
        ))
    }
}

fn member_key(lobby_id: &str, identity: Identity) -> String {
    format!("{lobby_id}:{identity}")
}

fn require_owner(ctx: &ReducerContext) -> Result<(), String> {
    let owner = ctx
        .db
        .control_config()
        .singleton_id()
        .find(CONFIG_ID)
        .ok_or("control configuration is missing")?
        .owner_identity;
    if ctx.sender() == owner {
        Ok(())
    } else {
        Err("only the lobby orchestrator may perform this action".to_owned())
    }
}

fn active_lobby_for_identity(ctx: &ReducerContext, identity: Identity) -> Option<String> {
    ctx.db
        .lobby_member()
        .iter()
        .filter(|member| member.identity == identity)
        .find_map(|member| {
            let lobby = ctx.db.lobby().lobby_id().find(&member.lobby_id)?;
            (!matches!(lobby.status, LobbyStatus::Failed | LobbyStatus::Cancelled))
                .then_some(lobby.lobby_id)
        })
}

fn delete_lobby_and_members(ctx: &ReducerContext, lobby_id: &str) {
    let member_keys = ctx
        .db
        .lobby_member()
        .iter()
        .filter(|member| member.lobby_id == lobby_id)
        .map(|member| member.member_key)
        .collect::<Vec<_>>();
    for key in member_keys {
        ctx.db.lobby_member().member_key().delete(&key);
    }
    ctx.db.lobby().lobby_id().delete(lobby_id.to_owned());
}

/// Pure post-leave status decision. Empty lobbies are deleted rather than kept
/// as Failed/Cancelled rows so membership cannot block future creates.
fn status_after_member_leave(
    status: LobbyStatus,
    member_count_after: u16,
) -> Result<LobbyStatus, ()> {
    if member_count_after == 0 {
        return Err(());
    }
    Ok(match status {
        LobbyStatus::Full => LobbyStatus::Open,
        other => other,
    })
}

#[spacetimedb::reducer(init)]
pub fn init(ctx: &ReducerContext) {
    ctx.db.control_config().insert(ControlConfig {
        singleton_id: CONFIG_ID,
        owner_identity: ctx.sender(),
    });
}

#[spacetimedb::reducer]
pub fn create_lobby(
    ctx: &ReducerContext,
    lobby_id: String,
    map_preset: LobbyMapPreset,
    player_count: u16,
    display_name: String,
) -> Result<(), String> {
    validate_lobby_id(&lobby_id)?;
    validate_player_count(player_count)?;
    let display_name = clean_display_name(&display_name)?;
    if let Some(existing) = ctx.db.lobby().lobby_id().find(&lobby_id) {
        if existing.creator_identity == ctx.sender()
            && existing.map_preset == map_preset
            && existing.player_count == player_count
            && matches!(
                existing.status,
                LobbyStatus::Pending
                    | LobbyStatus::Provisioning
                    | LobbyStatus::Open
                    | LobbyStatus::Full
            )
        {
            return Ok(());
        }
        return Err("lobby ID is already in use".to_owned());
    }
    if let Some(active) = active_lobby_for_identity(ctx, ctx.sender()) {
        return Err(format!(
            "leave active lobby {active} before creating another"
        ));
    }

    let now = timestamp_us(ctx);
    ctx.db.lobby().insert(Lobby {
        lobby_id: lobby_id.clone(),
        creator_identity: ctx.sender(),
        map_preset,
        player_count,
        member_count: 1,
        status: LobbyStatus::Pending,
        match_database: String::new(),
        failure_reason: String::new(),
        created_at_us: now,
        updated_at_us: now,
    });
    ctx.db.lobby_member().insert(LobbyMember {
        member_key: member_key(&lobby_id, ctx.sender()),
        lobby_id,
        identity: ctx.sender(),
        display_name,
        joined_at_us: now,
    });
    Ok(())
}

#[spacetimedb::reducer]
pub fn join_lobby(
    ctx: &ReducerContext,
    lobby_id: String,
    display_name: String,
) -> Result<(), String> {
    let display_name = clean_display_name(&display_name)?;
    if let Some(active) = active_lobby_for_identity(ctx, ctx.sender()) {
        if active == lobby_id {
            return Ok(());
        }
        return Err(format!(
            "leave active lobby {active} before joining another"
        ));
    }
    let mut lobby = ctx
        .db
        .lobby()
        .lobby_id()
        .find(&lobby_id)
        .ok_or("lobby does not exist")?;
    if lobby.status != LobbyStatus::Open {
        return Err("lobby is not open for joining".to_owned());
    }
    if lobby.member_count >= lobby.player_count {
        return Err("lobby is full".to_owned());
    }

    let now = timestamp_us(ctx);
    ctx.db.lobby_member().insert(LobbyMember {
        member_key: member_key(&lobby_id, ctx.sender()),
        lobby_id,
        identity: ctx.sender(),
        display_name,
        joined_at_us: now,
    });
    lobby.member_count = lobby.member_count.saturating_add(1);
    lobby.updated_at_us = now;
    if lobby.member_count == lobby.player_count {
        lobby.status = LobbyStatus::Full;
    }
    ctx.db.lobby().lobby_id().update(lobby);
    Ok(())
}

#[spacetimedb::reducer]
pub fn leave_lobby(ctx: &ReducerContext, lobby_id: String) -> Result<(), String> {
    let mut lobby = ctx
        .db
        .lobby()
        .lobby_id()
        .find(&lobby_id)
        .ok_or("lobby does not exist")?;
    let key = member_key(&lobby_id, ctx.sender());
    if ctx.db.lobby_member().member_key().find(&key).is_none() {
        return Ok(());
    }
    ctx.db.lobby_member().member_key().delete(&key);

    let member_count = lobby.member_count.saturating_sub(1);
    match status_after_member_leave(lobby.status, member_count) {
        Err(()) => {
            // Last member left — including creator leaving a Pending/Provisioning
            // lobby — so drop the row instead of leaving an orphan that blocks
            // create_lobby via active membership.
            delete_lobby_and_members(ctx, &lobby_id);
            Ok(())
        }
        Ok(status) => {
            lobby.member_count = member_count;
            lobby.status = status;
            lobby.updated_at_us = timestamp_us(ctx);
            ctx.db.lobby().lobby_id().update(lobby);
            Ok(())
        }
    }
}

#[spacetimedb::reducer]
pub fn begin_provision(ctx: &ReducerContext, lobby_id: String) -> Result<(), String> {
    require_owner(ctx)?;
    let mut lobby = ctx
        .db
        .lobby()
        .lobby_id()
        .find(&lobby_id)
        .ok_or("lobby does not exist")?;
    if lobby.status == LobbyStatus::Provisioning {
        return Ok(());
    }
    if lobby.status != LobbyStatus::Pending {
        return Err("lobby is not awaiting provisioning".to_owned());
    }
    lobby.status = LobbyStatus::Provisioning;
    lobby.updated_at_us = timestamp_us(ctx);
    ctx.db.lobby().lobby_id().update(lobby);
    Ok(())
}

#[spacetimedb::reducer]
pub fn complete_provision(
    ctx: &ReducerContext,
    lobby_id: String,
    match_database: String,
) -> Result<(), String> {
    require_owner(ctx)?;
    let mut lobby = ctx
        .db
        .lobby()
        .lobby_id()
        .find(&lobby_id)
        .ok_or("lobby does not exist")?;
    if matches!(lobby.status, LobbyStatus::Open | LobbyStatus::Full)
        && lobby.match_database == match_database
    {
        return Ok(());
    }
    if lobby.status != LobbyStatus::Provisioning {
        return Err("lobby is not being provisioned".to_owned());
    }
    if match_database.is_empty() {
        return Err("match database is required".to_owned());
    }
    lobby.status = LobbyStatus::Open;
    lobby.match_database = match_database;
    lobby.failure_reason.clear();
    lobby.updated_at_us = timestamp_us(ctx);
    ctx.db.lobby().lobby_id().update(lobby);
    Ok(())
}

#[spacetimedb::reducer]
pub fn fail_provision(
    ctx: &ReducerContext,
    lobby_id: String,
    reason: String,
) -> Result<(), String> {
    require_owner(ctx)?;
    let mut lobby = ctx
        .db
        .lobby()
        .lobby_id()
        .find(&lobby_id)
        .ok_or("lobby does not exist")?;
    if !matches!(
        lobby.status,
        LobbyStatus::Pending | LobbyStatus::Provisioning
    ) {
        return Err("lobby is not awaiting provisioning".to_owned());
    }
    lobby.status = LobbyStatus::Failed;
    lobby.failure_reason = reason.trim().chars().take(160).collect();
    lobby.updated_at_us = timestamp_us(ctx);
    ctx.db.lobby().lobby_id().update(lobby);
    Ok(())
}

#[spacetimedb::reducer]
pub fn remove_inactive_lobby(ctx: &ReducerContext, lobby_id: String) -> Result<(), String> {
    require_owner(ctx)?;
    let lobby = ctx
        .db
        .lobby()
        .lobby_id()
        .find(&lobby_id)
        .ok_or("lobby does not exist")?;
    if !matches!(lobby.status, LobbyStatus::Failed | LobbyStatus::Cancelled) {
        return Err("only failed or cancelled lobbies may be removed".to_owned());
    }
    delete_lobby_and_members(ctx, &lobby_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_public_lobby_ids() {
        assert!(validate_lobby_id("abc12345").is_ok());
        assert!(validate_lobby_id("too-short").is_err());
        assert!(validate_lobby_id("UPPERCASE1").is_err());
    }

    #[test]
    fn trims_and_bounds_display_names() {
        assert_eq!(clean_display_name("  Alice  ").unwrap(), "Alice");
        assert_eq!(clean_display_name(&"x".repeat(40)).unwrap().len(), 32);
        assert!(clean_display_name("   ").is_err());
    }

    #[test]
    fn last_member_leave_deletes_for_every_live_status() {
        for status in [
            LobbyStatus::Pending,
            LobbyStatus::Provisioning,
            LobbyStatus::Open,
            LobbyStatus::Full,
            LobbyStatus::Failed,
            LobbyStatus::Cancelled,
        ] {
            assert_eq!(status_after_member_leave(status, 0), Err(()));
        }
    }

    #[test]
    fn non_empty_leave_reopens_full_lobbies_only() {
        assert_eq!(
            status_after_member_leave(LobbyStatus::Full, 1),
            Ok(LobbyStatus::Open)
        );
        assert_eq!(
            status_after_member_leave(LobbyStatus::Open, 1),
            Ok(LobbyStatus::Open)
        );
        assert_eq!(
            status_after_member_leave(LobbyStatus::Pending, 1),
            Ok(LobbyStatus::Pending)
        );
        assert_eq!(
            status_after_member_leave(LobbyStatus::Provisioning, 1),
            Ok(LobbyStatus::Provisioning)
        );
    }
}
