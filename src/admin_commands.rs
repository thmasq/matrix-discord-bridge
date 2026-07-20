use crate::{cache::Cache, config::Config, db::Database, matrix_client::MatrixClient};
use clap::Args;
use clap::{Parser, Subcommand};
use ruma::events::room::message::RoomMessageEventContent;
use serde_json::Value;
use std::sync::Arc;

const MAX_MESSAGE_SIZE: usize = 30_000;
const HELP_TEMPLATE: &str = "\
    {about}

    {usage-heading} {usage}

    {all-args}";

pub enum CommandResponse {
    Text(String),
    Yaml(String),
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
        footer: Option<String>,
    },
    Terminal(String),
}

#[derive(Parser, Debug)]
#[command(name = "!", about = "Bridge Admin Commands", help_template = HELP_TEMPLATE)]
pub struct AdminCli {
    #[command(subcommand)]
    pub command: AdminCommand,
}

#[derive(Args, Debug, Clone)]
pub struct Pagination {
    #[arg(long, default_value_t = 1)]
    pub page: u32,
    #[arg(long, default_value_t = 10)]
    pub limit: u32,
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
    List(Pagination),
    /// Show bridge status for a room
    Status { matrix_room_id: String },
    /// Configure bridge settings
    Config {
        /// The Matrix room ID of the bridge to configure
        matrix_room_id: String,

        /// The setting to modify
        #[arg(value_parser = [
                "d2m_enabled", "m2d_enabled",
                "d2m_mod_deletions", "m2d_mod_deletions",
                "d2m_typing", "m2d_typing"
            ])]
        setting: Option<String>,

        /// The boolean value to apply
        #[arg(value_parser = ["true", "false", "1", "0", "yes", "no", "on", "off"])]
        value: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum InviteAction {
    /// List pending bot invites
    List(Pagination),
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
            let response = CommandResponse::Text(
                "Error: Invalid command format. Please check your quotes.".to_string(),
            );
            return self.send_command_response(room_id, response).await;
        };

        if args.is_empty() {
            return Ok(());
        }

        // Prepend dummy binary name for clap
        args.insert(0, "bot".to_string());

        let cli = match AdminCli::try_parse_from(args) {
            Ok(cli) => cli,
            Err(e) => {
                // Return beautiful terminal-like syntax highlighting for clap errors/help
                let response = CommandResponse::Terminal(e.to_string());
                return self.send_command_response(room_id, response).await;
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
                BridgeAction::List(pagination) => self.cmd_list(pagination).await, // Updated
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
                InviteAction::List(pagination) => self.cmd_invite_list(pagination).await, // Updated
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

        self.send_command_response(room_id, response).await
    }

    async fn send_command_response(
        &self,
        room_id: &str,
        response: CommandResponse,
    ) -> crate::error::Result<()> {
        let chunks = response.render_chunks();
        for (plain, html) in chunks {
            let content = RoomMessageEventContent::text_html(plain, html);
            let _ = self.matrix.send_message(room_id, content, None).await;
        }
        Ok(())
    }

    async fn cmd_list(&self, pagination: Pagination) -> CommandResponse {
        let limit = pagination.limit.clamp(1, 100);
        let offset = (pagination.page.saturating_sub(1)) * limit;

        match self.db.list_bridges_paginated(limit, offset).await {
            Ok(bridges) => {
                if bridges.is_empty() {
                    return CommandResponse::Text("No bridges configured.".to_string());
                }

                let total = self.db.count_bridges().await.unwrap_or(0);
                let total_pages = (total as f64 / limit as f64).ceil() as u32;

                let headers = vec![
                    "Matrix Room".to_string(),
                    "Discord Channel".to_string(),
                    "Settings".to_string(),
                ];
                let mut rows = Vec::new();

                for bridge in bridges {
                    rows.push(vec![
                        bridge.room_id,
                        bridge.channel_id,
                        format!(
                            "D2M: {} (ModDel: {}, TypingStatus: {}) | M2D: {} (ModDel: {}, TypingStatus: {})",
                            bridge.d2m_enabled,
                            bridge.d2m_mod_deletions,
                            bridge.d2m_typing,
                            bridge.m2d_enabled,
                            bridge.m2d_mod_deletions,
                            bridge.m2d_typing
                        ),
                    ]);
                }

                let footer = if total_pages > 1 {
                    Some(format!(
                        "Page {} of {} (Total: {}). Use --page <n> to navigate.",
                        pagination.page, total_pages, total
                    ))
                } else {
                    Some(format!("Total Bridges: {}", total))
                };

                CommandResponse::Table {
                    headers,
                    rows,
                    footer,
                }
            }
            Err(e) => CommandResponse::Text(format!("Error listing bridges: {e}")),
        }
    }

    async fn cmd_link(&self, _sender: &str, room_id: &str, channel_id: &str) -> CommandResponse {
        if !room_id.starts_with('!') || !room_id.contains(':') {
            return CommandResponse::Text(
                "Invalid Matrix room ID format. Should be like: !abc123:matrix.org".to_string(),
            );
        }

        if !channel_id.chars().all(|c| c.is_ascii_digit()) {
            return CommandResponse::Text(
                "Invalid Discord channel ID format. Should be numeric.".to_string(),
            );
        }

        match self.db.get_channel(room_id).await {
            Ok(Some(_)) => {
                return CommandResponse::Text(format!(
                    "Matrix room `{room_id}` is already bridged."
                ));
            }
            Ok(None) => {}
            Err(e) => return CommandResponse::Text(format!("Database error: {e}")),
        }

        match self.db.list_channels().await {
            Ok(channels) => {
                if channels.contains(&channel_id.to_string()) {
                    return CommandResponse::Text(format!(
                        "Discord channel `{channel_id}` is already bridged to another room."
                    ));
                }
            }
            Err(e) => return CommandResponse::Text(format!("Database error: {e}")),
        }

        match self.verify_discord_channel(channel_id).await {
            Ok(channel_info) => match self.db.add_room(room_id, channel_id).await {
                Ok(()) => {
                    let room_alias = self.matrix.matrixify_room(channel_id);
                    self.cache.m_rooms.insert(room_alias, room_id.to_string());
                    let _ = self.matrix.fetch_room_emojis(room_id, None).await;

                    CommandResponse::Text(format!(
                        "Successfully linked!\n\nMatrix: `{}`\nDiscord: #{} (`{}`)",
                        room_id, channel_info.name, channel_id
                    ))
                }
                Err(e) => CommandResponse::Text(format!("Failed to create bridge: {e}")),
            },
            Err(e) => CommandResponse::Text(format!(
                "Failed to verify Discord channel `{channel_id}`:\n{e}"
            )),
        }
    }

    async fn cmd_unlink(&self, room_id: &str) -> CommandResponse {
        match self.db.get_channel(room_id).await {
            Ok(Some(channel_id)) => match self.db.remove_room(room_id).await {
                Ok(()) => {
                    self.cache.remove_room_data(room_id);
                    CommandResponse::Text(format!(
                        "Successfully unlinked Matrix room `{room_id}` from Discord channel `{channel_id}`"
                    ))
                }
                Err(e) => CommandResponse::Text(format!("Failed to remove bridge: {e}")),
            },
            Ok(None) => CommandResponse::Text(format!("Matrix room `{room_id}` is not bridged.")),
            Err(e) => CommandResponse::Text(format!("Database error: {e}")),
        }
    }

    async fn cmd_status(&self, room_id: &str) -> CommandResponse {
        match self.db.get_channel(room_id).await {
            Ok(Some(channel_id)) => match self.verify_discord_channel(&channel_id).await {
                Ok(channel_info) => {
                    let emoji_count = self
                        .matrix
                        .get_room_emojis(room_id, None)
                        .await
                        .map_or(0, |e| e.len());

                    let yaml = format!(
                        "matrix_room: \"{}\"\n\
                                discord_channel:\n  \
                                  id: \"{}\"\n  \
                                  name: \"{}\"\n\
                                guild_id: \"{}\"\n\
                                custom_emojis_cached: {}\n\
                                status: \"Active\"",
                        room_id,
                        channel_id,
                        channel_info.name,
                        channel_info.guild_id.as_deref().unwrap_or("Unknown"),
                        emoji_count
                    );
                    CommandResponse::Yaml(yaml)
                }
                Err(e) => {
                    let yaml = format!(
                        "matrix_room: \"{}\"\n\
                                discord_channel: \"{}\"\n\
                                status: \"Discord channel not accessible\"\n\
                                error: \"{}\"",
                        room_id, channel_id, e
                    );
                    CommandResponse::Yaml(yaml)
                }
            },
            Ok(None) => CommandResponse::Text(format!("Matrix room `{room_id}` is not bridged.")),
            Err(e) => CommandResponse::Text(format!("Database error: {e}")),
        }
    }

    async fn cmd_verify(&self, channel_id: &str) -> CommandResponse {
        if !channel_id.chars().all(|c| c.is_ascii_digit()) {
            return CommandResponse::Text(
                "Invalid Discord channel ID format. Should be numeric.".to_string(),
            );
        }

        match self.verify_discord_channel(channel_id).await {
            Ok(channel_info) => {
                let channel_type_str = if channel_info.channel_type == 0 {
                    "Text"
                } else if channel_info.channel_type == 5 {
                    "News"
                } else {
                    "Other"
                };

                let yaml = format!(
                    "channel:\n  \
                          id: \"{}\"\n  \
                          name: \"{}\"\n\
                        type: \"{}\"\n\
                        guild_id: \"{}\"\n\
                        verified: true",
                    channel_info.id,
                    channel_info.name,
                    channel_type_str,
                    channel_info.guild_id.as_deref().unwrap_or("Unknown")
                );
                CommandResponse::Yaml(yaml)
            }
            Err(e) => {
                CommandResponse::Text(format!("Failed to verify channel `{channel_id}`:\n{e}"))
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

    async fn cmd_invite_list(&self, pagination: Pagination) -> CommandResponse {
        let limit = pagination.limit.clamp(1, 100);
        let offset = (pagination.page.saturating_sub(1)) * limit;

        let invites = match self.db.list_invites_paginated(limit, offset).await {
            Ok(invs) => invs,
            Err(e) => return CommandResponse::Text(format!("Database error: {e}")),
        };

        if invites.is_empty() {
            return CommandResponse::Text("No pending invites.".to_string());
        }

        let total = self.db.count_invites().await.unwrap_or(0);
        let total_pages = (total as f64 / limit as f64).ceil() as u32;

        let headers = vec![
            "List ID".to_string(),
            "Matrix Room".to_string(),
            "Room Name".to_string(),
            "Sender".to_string(),
        ];

        let mut rows = Vec::new();
        for inv in &invites {
            rows.push(vec![
                inv.id.to_string(),
                inv.room_id.clone(),
                inv.room_name
                    .clone()
                    .unwrap_or_else(|| "Unknown".to_string()),
                inv.sender.clone(),
            ]);
        }

        let footer = if total_pages > 1 {
            Some(format!(
                "Page {} of {} (Total: {}). Use --page <n> to navigate.",
                pagination.page, total_pages, total
            ))
        } else {
            Some(format!("Total Invites: {}", total))
        };

        CommandResponse::Table {
            headers,
            rows,
            footer,
        }
    }

    async fn cmd_invite_accept(&self, id_or_ranges: &str) -> CommandResponse {
        let invites = match self.db.list_invites().await {
            Ok(invs) => invs,
            Err(e) => return CommandResponse::Text(format!("Database error: {e}")),
        };

        let target_ids = Self::parse_indices(id_or_ranges, 99999999);

        let mut success_count = 0;
        let mut err_msgs = Vec::new();

        for inv in invites {
            if target_ids.contains(&(inv.id as usize)) {
                match self.matrix.join_room(&inv.room_id, None).await {
                    Ok(()) => {
                        let _ = self.db.remove_invite(inv.id).await;
                        success_count += 1;
                    }
                    Err(e) => {
                        err_msgs.push(format!("Failed to join {}: {}", inv.room_id, e));
                    }
                }
            }
        }

        let mut resp = format!("Accepted {success_count} invite(s).");
        if !err_msgs.is_empty() {
            resp.push_str("\nErrors:\n");
            resp.push_str(&err_msgs.join("\n"));
        }

        if success_count == 0 {
            return CommandResponse::Text(
                "No invites found matching the provided IDs.".to_string(),
            );
        }

        CommandResponse::Text(resp)
    }

    async fn cmd_invite_delete(&self, id_or_ranges: &str) -> CommandResponse {
        let target_ids = Self::parse_indices(id_or_ranges, 99999999);

        let invites = match self.db.list_invites().await {
            Ok(invs) => invs,
            Err(e) => return CommandResponse::Text(format!("Database error: {e}")),
        };

        let mut success_count = 0;
        for inv in invites {
            if target_ids.contains(&(inv.id as usize)) {
                let _ = self.matrix.leave_room(&inv.room_id, None).await;
                let _ = self.db.remove_invite(inv.id).await;
                success_count += 1;
            }
        }

        if success_count == 0 {
            return CommandResponse::Text(
                "No invites found matching the provided IDs.".to_string(),
            );
        }

        CommandResponse::Text(format!("Deleted {success_count} invite(s)."))
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
    ) -> CommandResponse {
        let bridge = match self.db.get_bridge(room_id).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                return CommandResponse::Text(format!("No bridge found for room `{room_id}`"));
            }
            Err(e) => return CommandResponse::Text(format!("Database error: {e}")),
        };

        if setting.is_none() || value.is_none() {
            let yaml = format!(
                "bridge_config:\n  \
                      room_id: \"{}\"\n  \
                      d2m_enabled: {}\n  \
                      m2d_enabled: {}\n  \
                      d2m_mod_deletions: {}\n  \
                      m2d_mod_deletions: {}\n  \
                      d2m_typing: {}\n  \
                      m2d_typing: {}",
                room_id,
                bridge.d2m_enabled,
                bridge.m2d_enabled,
                bridge.d2m_mod_deletions,
                bridge.m2d_mod_deletions,
                bridge.d2m_typing,
                bridge.m2d_typing
            );
            return CommandResponse::Yaml(yaml);
        }

        let setting = setting.unwrap();
        let value_str = value.unwrap().to_lowercase();
        let val_bool = match value_str.as_str() {
            "true" | "1" | "yes" | "on" => true,
            "false" | "0" | "no" | "off" => false,
            _ => return CommandResponse::Text("Value must be true or false".to_string()),
        };

        if let Err(e) = self
            .db
            .update_bridge_config(room_id, setting, val_bool)
            .await
        {
            return CommandResponse::Text(format!("Failed to update config: {e}"));
        }

        CommandResponse::Text(format!(
            "Updated `{setting}` to `{val_bool}` for bridge `{room_id}`"
        ))
    }

    fn cmd_debug_emojis(&self) -> CommandResponse {
        let d_emotes_count = self.cache.d_emotes.entry_count();
        let m_emotes_count = self.cache.m_emotes.entry_count();
        let m_custom_count = self.cache.m_custom_emojis.entry_count();

        let yaml = format!(
            "cache_stats:\n  \
    		  discord_emotes: {}\n  \
    		  matrix_cached_uploads: {}\n  \
    		  room_emoji_packs: {}\n\
                note: \"Detailed dump printed to application console log\"",
            d_emotes_count, m_emotes_count, m_custom_count
        );

        CommandResponse::Yaml(yaml)
    }
}

impl CommandResponse {
    pub fn render_chunks(self) -> Vec<(String, String)> {
        match self {
            CommandResponse::Text(text) => Self::chunk_text(&text, None),
            CommandResponse::Yaml(yaml) => Self::chunk_text(&yaml, Some("yaml")),
            CommandResponse::Terminal(term) => Self::chunk_text(&term, Some("bash")),
            CommandResponse::Table {
                headers,
                rows,
                footer,
            } => Self::chunk_table(&headers, &rows, footer.as_deref()),
        }
    }

    fn chunk_text(text: &str, lang: Option<&str>) -> Vec<(String, String)> {
        let mut chunks = Vec::new();
        let mut current_chunk = String::new();

        for line in text.lines() {
            if current_chunk.len() + line.len() > MAX_MESSAGE_SIZE {
                if !current_chunk.is_empty() {
                    chunks.push(Self::format_text_chunk(&current_chunk, lang));
                    current_chunk.clear();
                }

                if line.len() > MAX_MESSAGE_SIZE {
                    // Force split exceptionally long single lines safely along char boundaries
                    let mut current = line;
                    while current.len() > MAX_MESSAGE_SIZE {
                        let mut split_at = MAX_MESSAGE_SIZE;
                        while !current.is_char_boundary(split_at) {
                            split_at -= 1;
                        }
                        chunks.push(Self::format_text_chunk(&current[..split_at], lang));
                        current = &current[split_at..];
                    }
                    if !current.is_empty() {
                        current_chunk.push_str(current);
                        current_chunk.push('\n');
                    }
                } else {
                    current_chunk.push_str(line);
                    current_chunk.push('\n');
                }
            } else {
                current_chunk.push_str(line);
                current_chunk.push('\n');
            }
        }

        if !current_chunk.is_empty() {
            chunks.push(Self::format_text_chunk(&current_chunk, lang));
        }

        chunks
    }

    fn format_text_chunk(text: &str, lang: Option<&str>) -> (String, String) {
        let plain = text.trim_end().to_string();

        let html = if let Some(l) = lang {
            let encoded = html_escape::encode_text(&plain);
            format!("<pre><code class=\"language-{l}\">{}</code></pre>", encoded)
        } else {
            let plain_for_md = plain.replace('\n', "  \n");

            let mut options = pulldown_cmark::Options::empty();
            options.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
            options.insert(pulldown_cmark::Options::ENABLE_TABLES);

            let parser = pulldown_cmark::Parser::new_ext(&plain_for_md, options);
            let mut html_output = String::new();
            pulldown_cmark::html::push_html(&mut html_output, parser);

            html_output
        };

        (plain, html)
    }

    fn chunk_table(
        headers: &[String],
        rows: &[Vec<String>],
        footer: Option<&str>,
    ) -> Vec<(String, String)> {
        if rows.is_empty() {
            return vec![Self::format_text_chunk("No data to display.", None)];
        }

        let mut chunks = Vec::new();
        let mut current_rows = Vec::new();
        let mut current_size = 0;

        for row in rows {
            let row_size: usize = row.iter().map(|c| c.len() + 20).sum();
            if current_size + row_size > MAX_MESSAGE_SIZE && !current_rows.is_empty() {
                chunks.push(Self::format_table_chunk(headers, &current_rows, None));
                current_rows.clear();
                current_size = 0;
            }
            current_rows.push(row.clone());
            current_size += row_size;
        }

        if !current_rows.is_empty() {
            chunks.push(Self::format_table_chunk(headers, &current_rows, footer));
        }

        chunks
    }

    fn format_table_chunk(
        headers: &[String],
        rows: &[Vec<String>],
        footer: Option<&str>,
    ) -> (String, String) {
        let mut plain = String::new();
        plain.push_str(&headers.join(" | "));
        plain.push('\n');
        plain.push_str(
            &headers
                .iter()
                .map(|_| "---")
                .collect::<Vec<_>>()
                .join(" | "),
        );
        plain.push('\n');
        for row in rows {
            plain.push_str(&row.join(" | "));
            plain.push('\n');
        }

        let mut html = String::from("<table><thead><tr>");
        for h in headers {
            html.push_str(&format!("<th>{}</th>", html_escape::encode_text(h)));
        }
        html.push_str("</tr></thead><tbody>");
        for row in rows {
            html.push_str("<tr>");
            for col in row {
                html.push_str(&format!("<td>{}</td>", html_escape::encode_text(col)));
            }
            html.push_str("</tr>");
        }
        html.push_str("</tbody></table>");

        if let Some(f) = footer {
            plain.push_str("\n\n");
            plain.push_str(f);
            html.push_str(&format!("<br><em>{}</em>", html_escape::encode_text(f)));
        }

        (plain, html)
    }
}
