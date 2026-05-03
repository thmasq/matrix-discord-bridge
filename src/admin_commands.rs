use crate::{cache::Cache, config::Config, db::Database, matrix_client::MatrixClient};
use ruma::events::room::message::RoomMessageEventContent;
use serde_json::Value;
use std::sync::Arc;

pub struct AdminCommandHandler {
    config: Config,
    matrix: Arc<MatrixClient>,
    db: Database,
    cache: Cache,
    discord_http: reqwest::Client,
}

impl AdminCommandHandler {
    pub fn new(
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

        let parts: Vec<&str> = body.split_whitespace().collect();
        if parts.is_empty() {
            return Ok(());
        }

        let command = parts[0].trim_start_matches('!');

        let response = match command {
            "help" => self.cmd_help(),
            "list" => self.cmd_list().await,
            "link" => self.cmd_link(sender, &parts[1..]).await,
            "unlink" => self.cmd_unlink(&parts[1..]).await,
            "status" => self.cmd_status(&parts[1..]).await,
            "verify" => self.cmd_verify(&parts[1..]).await,
            "invite" => self.cmd_invite(&parts[1..]).await,
            _ => return Ok(()), // Unknown command, ignore
        };

        let content = RoomMessageEventContent::text_plain(response);
        let _ = self.matrix.send_message(room_id, content, None).await;

        Ok(())
    }

    fn cmd_help(&self) -> String {
        r#"**Bridge Admin Commands**

**!help** - Show this help message

**!list** - List all current bridges

**!link <matrix_room_id> <discord_channel_id>** - Create a new bridge
  Example: `!link !abc123:matrix.org 123456789012345678`

**!unlink <matrix_room_id>** - Remove a bridge
  Example: `!unlink !abc123:matrix.org`

**!status <matrix_room_id>** - Show bridge status for a room
  Example: `!status !abc123:matrix.org`

**!verify <discord_channel_id>** - Verify access to a Discord channel
  Example: `!verify 123456789012345678`

**!invite list** - List pending bot invites
**!invite accept <id>** - Accept a pending invite
**!invite delete <ids>** - Reject/delete invites (e.g., 2-4 or 5,6)"#
            .to_string()
    }

    async fn cmd_list(&self) -> String {
        match self.db.list_all_bridges().await {
            Ok(bridges) => {
                if bridges.is_empty() {
                    "No bridges configured.".to_string()
                } else {
                    let mut response = format!("**Active Bridges ({}):**\n\n", bridges.len());
                    for bridge in bridges {
                        response.push_str(&format!(
                            "• Matrix: `{}`\n  Discord: `{}`\n\n",
                            bridge.room_id, bridge.channel_id
                        ));
                    }
                    response
                }
            }
            Err(e) => format!("Error listing bridges: {}", e),
        }
    }

    async fn cmd_link(&self, _sender: &str, args: &[&str]) -> String {
        if args.len() < 2 {
            return "Usage: !link <matrix_room_id> <discord_channel_id>".to_string();
        }

        let room_id = args[0];
        let channel_id = args[1];

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
            Ok(Some(_)) => {
                return format!("Matrix room `{}` is already bridged.", room_id);
            }
            Ok(None) => {}
            Err(e) => {
                return format!("Database error: {}", e);
            }
        }

        // Check if Discord channel is already bridged
        match self.db.list_channels().await {
            Ok(channels) => {
                if channels.contains(&channel_id.to_string()) {
                    return format!(
                        "Discord channel `{}` is already bridged to another room.",
                        channel_id
                    );
                }
            }
            Err(e) => {
                return format!("Database error: {}", e);
            }
        }

        // Verify Discord channel access
        match self.verify_discord_channel(channel_id).await {
            Ok(channel_info) => {
                // Create the bridge
                match self.db.add_room(room_id, channel_id).await {
                    Ok(()) => {
                        // Update cache
                        let room_alias = self.matrix.matrixify_room(channel_id);
                        self.cache
                            .m_rooms
                            .write()
                            .insert(room_alias, room_id.to_string());

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
                            "✅ Successfully linked!\n\nMatrix: `{}`\nDiscord: #{} (`{}`)",
                            room_id, channel_info.name, channel_id
                        )
                    }
                    Err(e) => format!("Failed to create bridge: {}", e),
                }
            }
            Err(e) => {
                format!(
                    "❌ Failed to verify Discord channel `{}`:\n{}",
                    channel_id, e
                )
            }
        }
    }

    async fn cmd_unlink(&self, args: &[&str]) -> String {
        if args.is_empty() {
            return "Usage: !unlink <matrix_room_id>".to_string();
        }

        let room_id = args[0];

        // Check if bridge exists
        match self.db.get_channel(room_id).await {
            Ok(Some(channel_id)) => {
                match self.db.remove_room(room_id).await {
                    Ok(()) => {
                        // Clear cache
                        let room_alias = self.matrix.matrixify_room(&channel_id);
                        self.cache.m_rooms.write().remove(&room_alias);
                        self.cache.m_members.write().remove(room_id);
                        self.cache.m_custom_emojis.write().remove(room_id);

                        format!(
                            "✅ Successfully unlinked Matrix room `{}` from Discord channel `{}`",
                            room_id, channel_id
                        )
                    }
                    Err(e) => format!("Failed to remove bridge: {}", e),
                }
            }
            Ok(None) => format!("Matrix room `{}` is not bridged.", room_id),
            Err(e) => format!("Database error: {}", e),
        }
    }

    async fn cmd_status(&self, args: &[&str]) -> String {
        if args.is_empty() {
            return "Usage: !status <matrix_room_id>".to_string();
        }

        let room_id = args[0];

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
                            .map(|e| e.len())
                            .unwrap_or(0);

                        format!(
                            "**Bridge Status**\n\n\
                            Matrix Room: `{}`\n\
                            Discord Channel: #{} (`{}`)\n\
                            Guild: {}\n\
                            Custom Emojis Cached: {}\n\
                            Status: ✅ Active",
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
                            Matrix Room: `{}`\n\
                            Discord Channel: `{}`\n\
                            Status: ⚠️ Discord channel not accessible\n\
                            Error: {}",
                            room_id, channel_id, e
                        )
                    }
                }
            }
            Ok(None) => format!("Matrix room `{}` is not bridged.", room_id),
            Err(e) => format!("Database error: {}", e),
        }
    }

    async fn cmd_verify(&self, args: &[&str]) -> String {
        if args.is_empty() {
            return "Usage: !verify <discord_channel_id>".to_string();
        }

        let channel_id = args[0];

        if !channel_id.chars().all(|c| c.is_ascii_digit()) {
            return "Invalid Discord channel ID format. Should be numeric.".to_string();
        }

        match self.verify_discord_channel(channel_id).await {
            Ok(channel_info) => {
                format!(
                    "✅ **Discord Channel Verified**\n\n\
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
                format!("❌ Failed to verify channel `{}`:\n{}", channel_id, e)
            }
        }
    }

    async fn verify_discord_channel(&self, channel_id: &str) -> crate::error::Result<ChannelInfo> {
        let url = format!("https://discord.com/api/v10/channels/{}", channel_id);

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
                crate::error::BridgeError::Discord(serenity::Error::from(std::io::Error::other(
                    e.to_string(),
                )))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(crate::error::BridgeError::Discord(serenity::Error::from(
                std::io::Error::other(format!("Discord API error {}: {}", status, error_text)),
            )));
        }

        let channel_data: Value = response.json().await.map_err(|e| {
            crate::error::BridgeError::Matrix(format!("Failed to parse Discord response: {}", e))
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
            channel_type: channel_type as u8,
        })
    }

    async fn cmd_invite(&self, args: &[&str]) -> String {
        if args.is_empty() {
            return "Usage: `!invite <list|accept|delete> [args]`".to_string();
        }

        match args[0] {
            "list" => match self.db.list_invites().await {
                Ok(invites) => {
                    if invites.is_empty() {
                        "No pending invites.".to_string()
                    } else {
                        let mut response = "**Pending Invites:**\n\n".to_string();
                        for inv in invites {
                            response.push_str(&format!(
                                "`{}` - `{}` (from `{}`)\n",
                                inv.id, inv.room_id, inv.sender
                            ));
                        }
                        response
                    }
                }
                Err(e) => format!("Database error: {}", e),
            },
            "accept" => {
                if args.len() < 2 {
                    return "Usage: `!invite accept <id>`".to_string();
                }
                if let Ok(id) = args[1].parse::<i64>() {
                    match self.db.get_invite(id).await {
                        Ok(Some(invite)) => {
                            match self.matrix.join_room(&invite.room_id, None).await {
                                Ok(_) => {
                                    let _ = self.db.remove_invite(id).await;
                                    format!("✅ Accepted invite to `{}`", invite.room_id)
                                }
                                Err(e) => format!("❌ Failed to join room: {}", e),
                            }
                        }
                        Ok(None) => format!("❌ Invite ID `{}` not found.", id),
                        Err(e) => format!("Database error: {}", e),
                    }
                } else {
                    "Invalid invite ID format.".to_string()
                }
            }
            "delete" => {
                if args.len() < 2 {
                    return "Usage: `!invite delete <id_or_ranges>` (e.g. 1-3 or 4,5)".to_string();
                }

                // Parse the input (e.g. ["2-4,", "5"] becomes "2-4,5")
                let mut ids = std::collections::HashSet::new();
                let input = args[1..].join("");

                for part in input.split(',') {
                    if let Some((start, end)) = part.split_once('-') {
                        if let (Ok(s), Ok(e)) =
                            (start.trim().parse::<i64>(), end.trim().parse::<i64>())
                        {
                            for id in s..=e {
                                ids.insert(id);
                            }
                        }
                    } else if let Ok(id) = part.trim().parse::<i64>() {
                        ids.insert(id);
                    }
                }

                if ids.is_empty() {
                    return "No valid IDs provided.".to_string();
                }

                let mut success_count = 0;
                for id in ids {
                    if let Ok(Some(invite)) = self.db.get_invite(id).await {
                        let _ = self.matrix.leave_room(&invite.room_id, None).await;
                        let _ = self.db.remove_invite(id).await;
                        success_count += 1;
                    }
                }
                format!("🗑️ Deleted {} invite(s).", success_count)
            }
            _ => "Unknown invite action. Use `list`, `accept`, or `delete`.".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
struct ChannelInfo {
    id: String,
    name: String,
    guild_id: Option<String>,
    channel_type: u8,
}
