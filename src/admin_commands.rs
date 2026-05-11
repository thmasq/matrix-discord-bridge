use crate::{cache::Cache, config::Config, db::Database, matrix_client::MatrixClient};
use clap::{Parser, Subcommand};
use ruma::events::room::message::RoomMessageEventContent;
use serde_json::Value;
use std::fmt::Write;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(name = "!", about = "Bridge Admin Commands")]
pub struct AdminCli {
    #[command(subcommand)]
    pub command: AdminCommand,
}

#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// Manage room-to-channel links and config
    Bridge {
        #[command(subcommand)]
        action: BridgeAction,
    },
    /// Manage bot invitations
    Invite {
        #[command(subcommand)]
        action: InviteAction,
    },
    /// Utility commands
    Util {
        #[command(subcommand)]
        action: UtilAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum BridgeAction {
    /// Create a new bridge
    Link {
        matrix_room_id: String,
        discord_channel_id: String,
    },
    /// Remove a bridge
    Unlink { matrix_room_id: String },
    /// List all current bridges
    List,
    /// Show bridge status for a room
    Status { matrix_room_id: String },
    /// Configure bridge settings
    Config {
        matrix_room_id: String,
        setting: Option<String>,
        value: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum InviteAction {
    /// List pending bot invites
    List,
    /// Accept a pending invite (e.g., 1-3 or 4,5)
    Accept { id_or_ranges: String },
    /// Reject/delete invites (e.g., 1-3 or 4,5)
    Delete { id_or_ranges: String },
}

#[derive(Subcommand, Debug)]
pub enum UtilAction {
    /// Verify if bot has access to a Discord channel
    Verify {
        discord_channel_id: String,
    },
    DebugEmojis,
}

#[derive(Debug, Clone)]
struct ChannelInfo {
    id: String,
    name: String,
    guild_id: Option<String>,
    channel_type: u8,
}

pub struct AdminCommandHandler {
    config: Config,
    matrix: Arc<MatrixClient>,
    db: Database,
    cache: Cache,
    discord_http: reqwest::Client,
}

impl AdminCommandHandler {
    pub const fn new(
        config: Config,
        matrix: Arc<MatrixClient>,
        db: Database,
        cache: Cache,
        discord_http: reqwest::Client,
    ) -> Self {
        Self {
            config,
            matrix,
            db,
            cache,
            discord_http,
        }
    }

    pub async fn handle_command(
        &self,
        room_id: &str,
        sender: &str,
        body: &str,
    ) -> crate::error::Result<()> {
        // Only process commands from the config room
        if let Some(ref config_room) = self.config.config_room_id {
            if room_id != config_room {
                return Ok(());
            }
        } else {
            // No config room set, ignore
            return Ok(());
        }

        let body = body.trim();
        if !body.starts_with('!') {
            return Ok(());
        }

        let body_stripped = body.strip_prefix('!').unwrap_or(body);

        // Parse shell-like string with quotes
        let Some(mut args) = shlex::split(body_stripped) else {
            let response = "Error: Invalid command format. Please check your quotes.".to_string();
            return self.send_response(room_id, &response).await;
        };

        if args.is_empty() {
            return Ok(());
        }

        // Prepend dummy binary name for clap
        args.insert(0, "bot".to_string());

        let cli = match AdminCli::try_parse_from(args) {
            Ok(cli) => cli,
            Err(e) => {
                // Send clap auto-generated help or error messages inside a code block
                let response = format!(
                    "<pre><code>{}</code></pre>",
                    html_escape::encode_text(&e.to_string())
                );
                // Bypass MatrixClient processing since it's already pre-formatted HTML
                let plain_body = e.to_string();
                let content = RoomMessageEventContent::text_html(plain_body, response);
                let _ = self.matrix.send_message(room_id, content, None).await;
                return Ok(());
            }
        };

        let response = match cli.command {
            AdminCommand::Bridge { action } => match action {
                BridgeAction::Link {
                    matrix_room_id,
                    discord_channel_id,
                } => {
                    self.cmd_link(sender, &matrix_room_id, &discord_channel_id)
                        .await
                }
                BridgeAction::Unlink { matrix_room_id } => self.cmd_unlink(&matrix_room_id).await,
                BridgeAction::List => self.cmd_list().await,
                BridgeAction::Status { matrix_room_id } => self.cmd_status(&matrix_room_id).await,
                BridgeAction::Config {
                    matrix_room_id,
                    setting,
                    value,
                } => {
                    self.cmd_config(&matrix_room_id, setting.as_deref(), value.as_deref())
                        .await
                }
            },
            AdminCommand::Invite { action } => match action {
                InviteAction::List => self.cmd_invite_list().await,
                InviteAction::Accept { id_or_ranges } => {
                    self.cmd_invite_accept(&id_or_ranges).await
                }
                InviteAction::Delete { id_or_ranges } => {
                    self.cmd_invite_delete(&id_or_ranges).await
                }
            },
            AdminCommand::Util { action } => match action {
                UtilAction::Verify { discord_channel_id } => {
                    self.cmd_verify(&discord_channel_id).await
                }
                UtilAction::DebugEmojis => self.cmd_debug_emojis(),
            },
        };

        self.send_response(room_id, &response).await
    }

    async fn send_response(&self, room_id: &str, text: &str) -> crate::error::Result<()> {
        let (plain_body, html_body) =
            MatrixClient::process_for_matrix(text, &std::collections::HashMap::new());

        let content = RoomMessageEventContent::text_html(plain_body, html_body);
        let _ = self.matrix.send_message(room_id, content, None).await;
        Ok(())
    }

    async fn cmd_list(&self) -> String {
        match self.db.list_all_bridges().await {
            Ok(bridges) => {
                if bridges.is_empty() {
                    "No bridges configured.".to_string()
                } else {
                    let mut response = format!("**Active Bridges ({}):**\n\n", bridges.len());
                    for bridge in bridges {
                        let _ = write!(
                            response,
                            "• Matrix: `{}`\n  Discord: `{}`\n\n",
                            bridge.room_id, bridge.channel_id
                        );
                    }
                    response
                }
            }
            Err(e) => format!("Error listing bridges: {e}"),
        }
    }

    async fn cmd_link(&self, _sender: &str, room_id: &str, channel_id: &str) -> String {
        // Validate Matrix room ID format
        if !room_id.starts_with('!') || !room_id.contains(':') {
            return "Invalid Matrix room ID format. Should be like: !abc123:matrix.org".to_string();
        }

        // Validate Discord channel ID format
        if !channel_id.chars().all(|c| c.is_ascii_digit()) {
            return "Invalid Discord channel ID format. Should be numeric.".to_string();
        }

        // Check if room is already bridged
        match self.db.get_channel(room_id).await {
            Ok(Some(_)) => return format!("Matrix room `{room_id}` is already bridged."),
            Ok(None) => {}
            Err(e) => return format!("Database error: {e}"),
        }

        // Check if Discord channel is already bridged
        match self.db.list_channels().await {
            Ok(channels) => {
                if channels.contains(&channel_id.to_string()) {
                    return format!(
                        "Discord channel `{channel_id}` is already bridged to another room."
                    );
                }
            }
            Err(e) => return format!("Database error: {e}"),
        }

        // Verify Discord channel access
        match self.verify_discord_channel(channel_id).await {
            Ok(channel_info) => {
                // Create the bridge
                match self.db.add_room(room_id, channel_id).await {
                    Ok(()) => {
                        // Update cache
                        let room_alias = self.matrix.matrixify_room(channel_id);
                        self.cache.m_rooms.insert(room_alias, room_id.to_string());

                        // Prefetch custom emojis
                        match self.matrix.fetch_room_emojis(room_id).await {
                            Ok(emojis) => {
                                tracing::info!(
                                    "Prefetched {} custom emojis for room {}",
                                    emojis.len(),
                                    room_id
                                );
                            }
                            Err(e) => {
                                tracing::warn!("Failed to prefetch emojis: {}", e);
                            }
                        }

                        format!(
                            "Successfully linked!\n\nMatrix: `{}`\nDiscord: #{} (`{}`)",
                            room_id, channel_info.name, channel_id
                        )
                    }
                    Err(e) => format!("Failed to create bridge: {e}"),
                }
            }
            Err(e) => format!("Failed to verify Discord channel `{channel_id}`:\n{e}"),
        }
    }

    async fn cmd_unlink(&self, room_id: &str) -> String {
        // Check if bridge exists
        match self.db.get_channel(room_id).await {
            Ok(Some(channel_id)) => match self.db.remove_room(room_id).await {
                Ok(()) => {
                    self.cache.remove_room_data(room_id);

                    format!(
                        "Successfully unlinked Matrix room `{room_id}` from Discord channel `{channel_id}`"
                    )
                }
                Err(e) => format!("Failed to remove bridge: {e}"),
            },
            Ok(None) => format!("Matrix room `{room_id}` is not bridged."),
            Err(e) => format!("Database error: {e}"),
        }
    }

    async fn cmd_status(&self, room_id: &str) -> String {
        match self.db.get_channel(room_id).await {
            Ok(Some(channel_id)) => {
                // Get Discord channel info
                match self.verify_discord_channel(&channel_id).await {
                    Ok(channel_info) => {
                        // Get emoji count
                        let emoji_count = self
                            .matrix
                            .get_room_emojis(room_id)
                            .await
                            .map_or(0, |e| e.len());

                        format!(
                            "**Bridge Status**\n\n\
                            Matrix Room: `{}`\n\
                            Discord Channel: #{} (`{}`)\n\
                            Guild: {}\n\
                            Custom Emojis Cached: {}\n\
                            Status: Active",
                            room_id,
                            channel_info.name,
                            channel_id,
                            channel_info.guild_id.as_deref().unwrap_or("Unknown"),
                            emoji_count
                        )
                    }
                    Err(e) => {
                        format!(
                            "**Bridge Status**\n\n\
                            Matrix Room: `{room_id}`\n\
                            Discord Channel: `{channel_id}`\n\
                            Status: Discord channel not accessible\n\
                            Error: {e}"
                        )
                    }
                }
            }
            Ok(None) => format!("Matrix room `{room_id}` is not bridged."),
            Err(e) => format!("Database error: {e}"),
        }
    }

    async fn cmd_verify(&self, channel_id: &str) -> String {
        if !channel_id.chars().all(|c| c.is_ascii_digit()) {
            return "Invalid Discord channel ID format. Should be numeric.".to_string();
        }

        match self.verify_discord_channel(channel_id).await {
            Ok(channel_info) => {
                format!(
                    "**Discord Channel Verified**\n\n\
                    Channel: #{} (`{}`)\n\
                    Type: {}\n\
                    Guild: {}",
                    channel_info.name,
                    channel_info.id,
                    if channel_info.channel_type == 0 {
                        "Text"
                    } else if channel_info.channel_type == 5 {
                        "News"
                    } else {
                        "Other"
                    },
                    channel_info.guild_id.as_deref().unwrap_or("Unknown")
                )
            }
            Err(e) => {
                format!("Failed to verify channel `{channel_id}`:\n{e}")
            }
        }
    }

    async fn verify_discord_channel(&self, channel_id: &str) -> crate::error::Result<ChannelInfo> {
        let url = format!("https://discord.com/api/v10/channels/{channel_id}");

        let response = self
            .discord_http
            .get(&url)
            .header(
                "Authorization",
                format!("Bot {}", self.config.discord_token),
            )
            .send()
            .await
            .map_err(|e| {
                crate::error::BridgeError::Discord(Box::new(serenity::Error::from(
                    std::io::Error::other(e.to_string()),
                )))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(crate::error::BridgeError::Discord(Box::new(
                serenity::Error::from(std::io::Error::other(format!(
                    "Discord API error {status}: {error_text}"
                ))),
            )));
        }

        let channel_data: Value = response.json().await.map_err(|e| {
            crate::error::BridgeError::Matrix(format!("Failed to parse Discord response: {e}"))
        })?;

        let channel_type = channel_data["type"].as_u64().unwrap_or(999);
        if channel_type != 0 && channel_type != 5 {
            return Err(crate::error::BridgeError::Config(
                "Channel must be a text channel or news channel".to_string(),
            ));
        }

        Ok(ChannelInfo {
            id: channel_id.to_string(),
            name: channel_data["name"]
                .as_str()
                .unwrap_or("unknown")
                .to_string(),
            guild_id: channel_data["guild_id"].as_str().map(String::from),
            #[allow(clippy::cast_possible_truncation)]
            channel_type: channel_type as u8,
        })
    }

    async fn cmd_invite_list(&self) -> String {
        let invites = match self.db.list_invites().await {
            Ok(invs) => invs,
            Err(e) => return format!("Database error: {e}"),
        };

        if invites.is_empty() {
            "No pending invites.".to_string()
        } else {
            let mut response = "**Pending Invites:**\n\n".to_string();
            for inv in &invites {
                let name_part = inv
                    .room_name
                    .as_deref()
                    .map(|n| format!(" `{n}`"))
                    .unwrap_or_default();

                let _ = writeln!(
                    response,
                    "- **room**: \"{}\", **id**: \"{}\" (from `{}`)",
                    name_part, inv.room_id, inv.sender
                );
            }
            response
        }
    }

    async fn cmd_invite_accept(&self, id_or_ranges: &str) -> String {
        let invites = match self.db.list_invites().await {
            Ok(invs) => invs,
            Err(e) => return format!("Database error: {e}"),
        };

        let indices = Self::parse_indices(id_or_ranges, invites.len());
        if indices.is_empty() {
            return "No valid invites found for the given range.".to_string();
        }

        let mut success_count = 0;
        let mut err_msgs = Vec::new();

        for &idx in &indices {
            let invite = &invites[idx - 1];
            match self.matrix.join_room(&invite.room_id, None).await {
                Ok(()) => {
                    let _ = self.db.remove_invite(invite.id).await;
                    success_count += 1;
                }
                Err(e) => {
                    err_msgs.push(format!("Failed to join {}: {}", invite.room_id, e));
                }
            }
        }

        let mut resp = format!("Accepted {success_count} invite(s).");
        if !err_msgs.is_empty() {
            resp.push_str("\nErrors:\n");
            resp.push_str(&err_msgs.join("\n"));
        }
        resp
    }

    async fn cmd_invite_delete(&self, id_or_ranges: &str) -> String {
        let invites = match self.db.list_invites().await {
            Ok(invs) => invs,
            Err(e) => return format!("Database error: {e}"),
        };

        let indices = Self::parse_indices(id_or_ranges, invites.len());
        if indices.is_empty() {
            return "No valid invites found for the given range.".to_string();
        }

        let mut success_count = 0;
        for &idx in &indices {
            let invite = &invites[idx - 1];
            let _ = self.matrix.leave_room(&invite.room_id, None).await;
            let _ = self.db.remove_invite(invite.id).await;
            success_count += 1;
        }
        format!("Deleted {success_count} invite(s).")
    }

    fn parse_indices(input: &str, max_val: usize) -> Vec<usize> {
        let mut indices = std::collections::HashSet::new();
        for part in input.split(',') {
            if let Some((start, end)) = part.split_once('-') {
                if let (Ok(s), Ok(e)) = (start.trim().parse::<usize>(), end.trim().parse::<usize>())
                {
                    for i in s..=e {
                        if i > 0 && i <= max_val {
                            indices.insert(i);
                        }
                    }
                }
            } else if let Ok(i) = part.trim().parse::<usize>()
                && i > 0
                && i <= max_val
            {
                indices.insert(i);
            }
        }
        let mut result: Vec<usize> = indices.into_iter().collect();
        result.sort_unstable();
        result
    }

    async fn cmd_config(
        &self,
        room_id: &str,
        setting: Option<&str>,
        value: Option<&str>,
    ) -> String {
        let bridge = match self.db.get_bridge(room_id).await {
            Ok(Some(b)) => b,
            Ok(None) => return format!("No bridge found for room `{room_id}`"),
            Err(e) => return format!("Database error: {e}"),
        };

        if setting.is_none() || value.is_none() {
            return format!(
                "**Configuration for {}**\n\
                `d2m_enabled`: {}\n\
                `m2d_enabled`: {}\n\
                `d2m_mod_deletions`: {}\n\
                `m2d_mod_deletions`: {}\n\
                `d2m_typing`: {}\n\
                `m2d_typing`: {}",
                room_id,
                bridge.d2m_enabled,
                bridge.m2d_enabled,
                bridge.d2m_mod_deletions,
                bridge.m2d_mod_deletions,
                bridge.d2m_typing,
                bridge.m2d_typing
            );
        }

        let setting = setting.unwrap();
        let value_str = value.unwrap().to_lowercase();
        let val_bool = match value_str.as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            _ => return "Value must be true or false".to_string(),
        };

        if let Err(e) = self
            .db
            .update_bridge_config(room_id, setting, val_bool)
            .await
        {
            return format!("Failed to update config: {e}");
        }

        format!("Updated `{setting}` to `{val_bool}` for bridge `{room_id}`")
    }

    fn cmd_debug_emojis(&self) -> String {
        tracing::info!("=== BEGIN EMOJI CACHE DUMP ===");

        let d_emotes_count = self.cache.d_emotes.entry_count();
        tracing::info!("d_emotes (Discord emojis) count: {}", d_emotes_count);
        for (k, v) in &self.cache.d_emotes {
            tracing::info!("  {} -> {}", k, v);
        }

        let m_emotes_count = self.cache.m_emotes.entry_count();
        tracing::info!("m_emotes (Matrix cached uploads) count: {}", m_emotes_count);
        for (k, v) in &self.cache.m_emotes {
            tracing::info!("  {} -> {}", k, v);
        }

        let m_custom_count = self.cache.m_custom_emojis.entry_count();
        tracing::info!(
            "m_custom_emojis (Room state packs) count: {}",
            m_custom_count
        );
        for (room, emojis) in &self.cache.m_custom_emojis {
            tracing::info!("  Room: {}", room);
            for (shortcode, mxc) in emojis {
                tracing::info!("    {} -> {}", shortcode, mxc);
            }
        }
        tracing::info!("=== END EMOJI CACHE DUMP ===");

        format!(
            "**Cache Dumped to Console!**\nDiscord Emotes: `{d_emotes_count}`\nMatrix Cached Uploads: `{m_emotes_count}`\nRoom Emoji Packs: `{m_custom_count}`"
        )
    }
}
