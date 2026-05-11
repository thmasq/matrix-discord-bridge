use crate::{cache::Cache, config::Config, db::Database, matrix_client::MatrixClient};
use ruma::events::room::message::RoomMessageEventContent;
use serenity::{
    Client,
    all::{
        ChannelId, GuildId, MessageId, Reaction, ReactionType, StickerFormatType,
        UserId as DiscordUserId,
    },
    async_trait,
    client::{Context, EventHandler},
    model::{
        channel::Message,
        event::MessageUpdateEvent,
        gateway::Ready,
        guild::{Guild, Member},
    },
};
use std::{
    collections::HashMap,
    sync::{Arc, OnceLock},
};
use std::{fmt::Write, hash::Hasher};
use twox_hash::XxHash32;

static ROLE_REGEX: OnceLock<regex::Regex> = OnceLock::new();
static CHANNEL_REGEX: OnceLock<regex::Regex> = OnceLock::new();
static EMOTE_REGEX: OnceLock<regex::Regex> = OnceLock::new();
static REPLY_REGEX: OnceLock<regex::Regex> = OnceLock::new();
static PLAIN_EMOTE_REGEX: OnceLock<regex::Regex> = OnceLock::new();

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct AttachmentInfo {
    pub url: String,
    pub filename: String,
    pub content_type: Option<String>,
    pub size: u32,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[allow(dead_code)]
pub struct DiscordHandler {
    matrix: Arc<MatrixClient>,
    db: Database,
    cache: Cache,
    config: Config,
}

impl DiscordHandler {
    pub const fn new(
        matrix: Arc<MatrixClient>,
        db: Database,
        cache: Cache,
        config: Config,
    ) -> Self {
        Self {
            matrix,
            db,
            cache,
            config,
        }
    }

    fn hash_webhook_name(s: &str) -> u64 {
        let mut hasher = XxHash32::with_seed(0);
        hasher.write(s.as_bytes());
        hasher.finish()
    }

    fn get_discriminator(user: &serenity::model::user::User) -> u16 {
        user.discriminator.map_or(0, std::num::NonZero::get)
    }

    fn process_discord_message(&self, message: &Message) -> (String, HashMap<String, String>) {
        let mut content = message.content.clone();
        let mut emotes = HashMap::new();

        // Process user mentions
        for user in &message.mentions {
            let mention = format!("<@{}>", user.id);
            let mention_nick = format!("<@!{}>", user.id);
            content = content.replace(&mention, &format!("@{}", user.name));
            content = content.replace(&mention_nick, &format!("@{}", user.name));
        }

        // Process role mentions: <@&123456> -> @role-name
        let role_regex = ROLE_REGEX.get_or_init(|| regex::Regex::new(r"<@&(\d+)>").unwrap());
        for cap in role_regex.captures_iter(&message.content.clone()) {
            if let Some(role_id_match) = cap.get(1) {
                let role_id = role_id_match.as_str();

                // Try to get role name from cache
                if let Some(guild_id) = message.guild_id {
                    let guild_id_str = guild_id.to_string();
                    let role_name = self
                        .cache
                        .d_roles
                        .get(&guild_id_str)
                        .and_then(|guild_roles| guild_roles.get(role_id).cloned());

                    if let Some(name) = role_name {
                        content = content.replace(&cap[0], &format!("@{name}"));
                    } else {
                        content = content.replace(&cap[0], &format!("@role-{role_id}"));
                    }
                } else {
                    content = content.replace(&cap[0], "@deleted-role");
                }
            }
        }

        // Process channel mentions: <#123456> -> #channel-name
        let channel_regex = CHANNEL_REGEX.get_or_init(|| regex::Regex::new(r"<#(\d+)>").unwrap());
        for cap in channel_regex.captures_iter(&message.content.clone()) {
            if let Some(channel_id_match) = cap.get(1) {
                let channel_id = channel_id_match.as_str();

                // Try to get channel name from cache
                if let Some(guild_id) = message.guild_id {
                    let guild_id_str = guild_id.to_string();
                    let channel_name = self
                        .cache
                        .d_channels
                        .get(&guild_id_str)
                        .and_then(|guild_channels| guild_channels.get(channel_id).cloned());

                    if let Some(name) = channel_name {
                        content = content.replace(&cap[0], &format!("#{name}"));
                    } else {
                        content = content.replace(&cap[0], &format!("#channel-{channel_id}"));
                    }
                } else {
                    content = content.replace(&cap[0], "#deleted-channel");
                }
            }
        }

        // Process emotes: <:name:id> or <a:name:id>
        let emote_regex =
            EMOTE_REGEX.get_or_init(|| regex::Regex::new(r"<a?:(\w+):(\d+)>").unwrap());
        for cap in emote_regex.captures_iter(&message.content) {
            let name = cap.get(1).unwrap().as_str();
            let id = cap.get(2).unwrap().as_str();
            emotes.insert(name.to_string(), id.to_string());
        }
        content = emote_regex.replace_all(&content, ":$1:").to_string();

        // Note: Attachments are handled separately in process_attachments
        // Note: Stickers are handled separately in process_stickers

        (content, emotes)
    }

    fn process_attachments(message: &Message) -> Vec<AttachmentInfo> {
        message
            .attachments
            .iter()
            .map(|att| AttachmentInfo {
                url: att.url.clone(),
                filename: att.filename.clone(),
                content_type: att.content_type.clone(),
                size: att.size,
                width: att.width,
                height: att.height,
            })
            .collect()
    }

    fn process_stickers(message: &Message) -> Vec<String> {
        message
            .sticker_items
            .iter()
            .filter(|s| s.format_type != StickerFormatType::Lottie)
            .map(|s| format!("https://cdn.discordapp.com/stickers/{}.png", s.id))
            .collect()
    }

    fn process_embeds(message: &Message) -> String {
        if message.embeds.is_empty() {
            return String::new();
        }

        let mut embed_text = String::new();

        for embed in &message.embeds {
            embed_text.push_str("\n\n---\n");

            if let Some(author) = &embed.author {
                let _ = writeln!(embed_text, "**{}**", author.name);
            }

            if let Some(title) = &embed.title {
                if let Some(url) = &embed.url {
                    let _ = writeln!(embed_text, "**[{title}]({url})**");
                } else {
                    let _ = writeln!(embed_text, "**{title}**");
                }
            }

            if let Some(description) = &embed.description {
                let _ = writeln!(embed_text, "{description}");
            }

            for field in &embed.fields {
                let _ = writeln!(embed_text, "\n**{}**\n{}", field.name, field.value);
            }

            if let Some(footer) = &embed.footer {
                let _ = write!(embed_text, "\n\n*{}*", footer.text);
            }

            if let Some(image) = &embed.image {
                let _ = write!(embed_text, "\n{}", image.url);
            }

            if let Some(thumbnail) = &embed.thumbnail {
                let _ = write!(embed_text, "\n{}", thumbnail.url);
            }
        }

        embed_text
    }

    async fn sync_profile(
        &self,
        user_id: DiscordUserId,
        username: &str,
        discriminator: u16,
        avatar_url: &str,
        hashed: Option<&str>,
    ) -> crate::error::Result<()> {
        let mxid = self.matrix.matrixify_user(&user_id.to_string(), hashed);

        // Check if user exists and fetch their current profile
        let Some(profile) = self.db.fetch_user(&mxid).await? else {
            // User doesn't exist yet, will be created on first message
            return Ok(());
        };

        let display_name = if discriminator == 0 {
            // New username system (no discriminator)
            username.to_string()
        } else {
            // Legacy username system
            format!("{username}#{discriminator:04}")
        };

        let mut updated = false;

        // Update display name if changed
        if profile.username.as_deref() != Some(&display_name) {
            match self.matrix.set_display_name(&mxid, &display_name).await {
                Ok(()) => {
                    tracing::info!("Updated display name for {} to {}", mxid, display_name);
                    updated = true;
                }
                Err(e) => {
                    tracing::error!("Failed to update display name for {}: {}", mxid, e);
                }
            }
        }

        // Update avatar if changed
        if profile.avatar_url.as_deref() != Some(avatar_url) {
            match self.matrix.set_avatar(&mxid, avatar_url).await {
                Ok(()) => {
                    tracing::info!("Updated avatar for {}", mxid);
                    updated = true;
                }
                Err(e) => {
                    tracing::error!("Failed to update avatar for {}: {}", mxid, e);
                }
            }
        }

        if updated {
            tracing::debug!("Profile sync completed for {}", mxid);
        }

        Ok(())
    }

    async fn ensure_user_in_room(
        &self,
        _ctx: &Context,
        message: &Message,
        room_id: &str,
    ) -> crate::error::Result<String> {
        let hashed = if message.webhook_id.is_some() {
            Some(format!(
                "{:x}",
                Self::hash_webhook_name(&message.author.name)
            ))
        } else {
            None
        };

        let mxid = self
            .matrix
            .matrixify_user(&message.author.id.to_string(), hashed.as_deref());

        // Check if user exists in database
        let user_exists = self.db.fetch_user(&mxid).await?.is_some();

        if !user_exists {
            // Register the user
            tracing::info!("Registering new puppet user: {}", mxid);
            self.matrix.register_user(&mxid).await?;

            let discriminator = Self::get_discriminator(&message.author);

            let display_name = if discriminator == 0 {
                message.author.name.clone()
            } else {
                format!("{}#{:04}", message.author.name, discriminator)
            };

            self.matrix.set_display_name(&mxid, &display_name).await?;

            let avatar = message.author.face();
            if let Err(e) = self.matrix.set_avatar(&mxid, &avatar).await {
                tracing::warn!("Failed to set avatar for {}: {}", mxid, e);
            }
        } else if message.webhook_id.is_some() {
            // For webhook messages, always sync profile
            let discriminator = Self::get_discriminator(&message.author);
            let avatar = message.author.face();
            let _ = self
                .sync_profile(
                    message.author.id,
                    &message.author.name,
                    discriminator,
                    &avatar,
                    hashed.as_deref(),
                )
                .await;
        }

        // Check if user is already in the room
        let is_in_room = self
            .matrix
            .is_user_in_room(room_id, &mxid)
            .await
            .unwrap_or(false);

        if is_in_room {
            tracing::debug!("User {} already in room {}", mxid, room_id);
        } else {
            // User not in room, invite and join
            if let Err(e) = self.matrix.send_invite(room_id, &mxid).await {
                tracing::debug!("Invite failed for {} (may already be invited): {}", mxid, e);
            }

            if let Err(e) = self.matrix.join_room(room_id, Some(&mxid)).await {
                tracing::debug!("Join failed for {} (may already be in room): {}", mxid, e);
            }
        }

        Ok(mxid)
    }

    async fn resolve_bridge(&self, channel_id: &str) -> Option<crate::db::BridgedRoom> {
        let room_id = self.resolve_room_id(channel_id).await?;
        self.db.get_bridge(&room_id).await.ok().flatten()
    }

    async fn resolve_room_id(&self, channel_id: &str) -> Option<String> {
        let room_alias = self.matrix.matrixify_room(channel_id);

        // Check cache first
        if let Some(room_id) = self.cache.m_rooms.get(&room_alias) {
            return Some(room_id);
        }

        match self.db.get_room_by_channel(channel_id).await {
            Ok(Some(room_id)) => {
                self.cache.m_rooms.insert(room_alias, room_id.clone());
                Some(room_id)
            }
            Ok(None) => {
                tracing::debug!(
                    "No Matrix room found in database for Discord channel {}",
                    channel_id
                );
                None
            }
            Err(e) => {
                tracing::error!(
                    "Database error resolving room for channel {}: {}",
                    channel_id,
                    e
                );
                None
            }
        }
    }

    fn is_bridge_webhook(&self, message: &Message) -> bool {
        if let Some(webhook_id) = message.webhook_id {
            for (_key, info) in &self.cache.d_webhooks {
                if info.id == webhook_id.to_string() {
                    return true;
                }
            }
        }
        false
    }

    async fn send_attachments_to_matrix(
        &self,
        room_id: &str,
        mxid: &str,
        attachments: Vec<AttachmentInfo>,
    ) -> crate::error::Result<Vec<String>> {
        let mut event_ids = Vec::new();

        for attachment in attachments {
            match self.matrix.send_media(room_id, mxid, &attachment).await {
                Ok(event_id) => {
                    tracing::info!(
                        "Sent attachment {} to Matrix as {}",
                        attachment.filename,
                        event_id
                    );
                    event_ids.push(event_id);
                }
                Err(e) => {
                    tracing::error!("Failed to send attachment {}: {}", attachment.filename, e);
                    // Continue with other attachments
                }
            }
        }

        Ok(event_ids)
    }

    fn strip_reply_fallback(body: &str) -> String {
        let mut result = String::new();
        let mut in_fallback = true;

        for line in body.lines() {
            if !line.starts_with("> ") {
                in_fallback = false;
            }
            if !in_fallback {
                if !result.is_empty() {
                    result.push('\n');
                }
                result.push_str(line);
            }
        }

        result.trim().to_string()
    }

    /// Strip <mx-reply>...</mx-reply> tags from formatted HTML body
    fn strip_reply_fallback_html(html: &str) -> String {
        // Use regex to strip <mx-reply>...</mx-reply> tags (with DOTALL flag)
        let regex =
            REPLY_REGEX.get_or_init(|| regex::Regex::new(r"(?s)<mx-reply>.*?</mx-reply>").unwrap());
        regex.replace_all(html, "").to_string()
    }

    #[allow(clippy::too_many_lines)]
    async fn create_matrix_message_content_with_emojis(
        &self,
        room_id: &str,
        content: &str,
        discord_emotes: &HashMap<String, String>,
        reply_to_event_id: Option<String>,
    ) -> RoomMessageEventContent {
        // Get Matrix room's custom emojis
        let room_emojis = self
            .matrix
            .get_room_emojis(room_id)
            .await
            .unwrap_or_default();

        // Build a map of matched emojis (name -> MXC URL)
        let mut matched_emojis = HashMap::new();

        for (emote_name, discord_id) in discord_emotes {
            // Check if Matrix room has a custom emoji with the same name
            if let Some(mxc_url) = room_emojis.get(emote_name) {
                // Found a match! Use the Matrix custom emoji
                matched_emojis.insert(emote_name.clone(), mxc_url.clone());
                tracing::debug!(
                    "Matched Discord emoji :{}: to Matrix emoji {}",
                    emote_name,
                    mxc_url
                );
            } else {
                // Upload the Discord emoji to Matrix
                let discord_url = format!("https://cdn.discordapp.com/emojis/{discord_id}.png");

                // Check if we've already uploaded this emoji
                let cached_mxc = self.cache.m_emotes.get(emote_name);

                if let Some(mxc) = cached_mxc {
                    matched_emojis.insert(emote_name.clone(), mxc);
                } else {
                    // Upload the Discord emoji to Matrix
                    match self.matrix.upload_from_url(&discord_url).await {
                        Ok(mxc_url) => {
                            self.cache
                                .m_emotes
                                .insert(emote_name.clone(), mxc_url.clone());
                            matched_emojis.insert(emote_name.clone(), mxc_url);
                            tracing::debug!(
                                "Uploaded Discord emoji :{}: to Matrix as {}",
                                emote_name,
                                matched_emojis.get(emote_name).unwrap()
                            );
                        }
                        Err(e) => {
                            tracing::warn!("Failed to upload Discord emoji {}: {}", emote_name, e);
                        }
                    }
                }
            }
        }

        let plain_emote_regex =
            PLAIN_EMOTE_REGEX.get_or_init(|| regex::Regex::new(r":([a-zA-Z0-9_-]+):").unwrap());
        for cap in plain_emote_regex.captures_iter(content) {
            let emote_name = cap.get(1).unwrap().as_str();

            if !matched_emojis.contains_key(emote_name)
                && let Some(mxc_url) = room_emojis.get(emote_name)
            {
                matched_emojis.insert(emote_name.to_string(), mxc_url.clone());
            }
        }

        // Process markdown and emotes
        let (plain_body, formatted_body) =
            MatrixClient::process_for_matrix(content, &matched_emojis);

        if let Some(event_id) = reply_to_event_id {
            // Fetch the original event to strip fallbacks
            let (reply_sender, reply_body, reply_formatted) =
                match self.matrix.get_event(room_id, &event_id).await {
                    Ok(event) => {
                        let stripped_body = Self::strip_reply_fallback(&event.body);
                        let stripped_formatted = event.formatted_body.map_or_else(
                            || stripped_body.clone(),
                            |fb| Self::strip_reply_fallback_html(&fb),
                        );

                        (event.sender, stripped_body, stripped_formatted)
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch event {} for reply: {}", event_id, e);
                        // Fallback to simple reply without content preview
                        ("unknown".to_string(), "...".to_string(), "...".to_string())
                    }
                };

            // Create the reply fallback for plain text
            let reply_fallback_plain = format!("> <{reply_sender}> {reply_body}\n\n{plain_body}");

            // Escape HTML for the content if it's plain text
            let escape_html = |s: &str| -> String {
                s.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
                    .replace('"', "&quot;")
                    .replace('\'', "&#39;")
            };

            let reply_content_html = if formatted_body == plain_body {
                escape_html(&plain_body)
            } else {
                formatted_body.clone()
            };

            // Create the reply fallback for HTML
            let reply_fallback_html = format!(
                "<mx-reply><blockquote>\
                <a href=\"https://matrix.to/#/{room_id}/{event_id}\">In reply to</a> \
                <a href=\"https://matrix.to/#/{reply_sender}\">{reply_sender}</a>\
                <br />{reply_formatted}\
                </blockquote></mx-reply>{reply_content_html}"
            );

            let content =
                RoomMessageEventContent::text_html(reply_fallback_plain, reply_fallback_html);

            match serde_json::to_value(&content) {
                Ok(mut json) => {
                    json["m.relates_to"] = serde_json::json!({
                        "m.in_reply_to": {
                            "event_id": event_id
                        }
                    });

                    match serde_json::from_value(json) {
                        Ok(updated_content) => updated_content,
                        Err(e) => {
                            tracing::warn!("Failed to deserialize reply content: {}", e);
                            content
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to serialize content for reply: {}", e);
                    content
                }
            }
        } else if formatted_body == plain_body {
            RoomMessageEventContent::text_plain(&plain_body)
        } else {
            RoomMessageEventContent::text_html(&plain_body, formatted_body)
        }
    }
}

#[async_trait]
#[allow(clippy::too_many_lines)]
impl EventHandler for DiscordHandler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        tracing::info!("Discord bot connected as {}", ready.user.name);
    }

    async fn guild_create(&self, ctx: Context, guild: Guild, _is_new: Option<bool>) {
        tracing::info!("Guild available: {} ({})", guild.name, guild.id);

        let guild_id_str = guild.id.to_string();

        // Cache emotes
        match guild.id.emojis(&ctx.http).await {
            Ok(emojis) => {
                let emote_count = emojis.len();
                for emoji in emojis {
                    let emote_str = if emoji.animated {
                        format!("<a:{}:{}>", emoji.name, emoji.id)
                    } else {
                        format!("<:{}:{}>", emoji.name, emoji.id)
                    };
                    self.cache.d_emotes.insert(emoji.name.clone(), emote_str);
                }
                tracing::info!("Cached {} emotes from guild {}", emote_count, guild.id);
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to fetch emojis via HTTP for guild {}: {}",
                    guild.id,
                    e
                );
            }
        }

        // Cache roles
        let role_count = guild.roles.len();
        let mut guild_roles = HashMap::new();
        for (role_id, role) in &guild.roles {
            guild_roles.insert(role_id.to_string(), role.name.clone());
        }
        self.cache.d_roles.insert(guild_id_str.clone(), guild_roles);
        tracing::debug!("Cached {} roles from guild {}", role_count, guild.id);

        // Cache channels
        let channel_count = guild.channels.len();
        let mut guild_channels = HashMap::new();
        for (channel_id, channel) in &guild.channels {
            guild_channels.insert(channel_id.to_string(), channel.name.clone());
        }
        self.cache
            .d_channels
            .insert(guild_id_str.clone(), guild_channels);
        tracing::debug!("Cached {} channels from guild {}", channel_count, guild.id);

        // Sync profiles for all members (async in background)
        let member_count = guild.members.len();

        tracing::info!(
            "Syncing profiles for {} members in guild {}",
            member_count,
            guild.id
        );

        for (user_id, member) in guild.members {
            let discriminator = Self::get_discriminator(&member.user);
            let avatar = member.user.face();

            if let Err(e) = self
                .sync_profile(user_id, &member.user.name, discriminator, &avatar, None)
                .await
            {
                tracing::debug!("Failed to sync profile for {}: {}", user_id, e);
            }
        }

        tracing::info!("Completed profile sync for guild {}", guild.id);
    }

    async fn guild_delete(
        &self,
        _ctx: Context,
        incomplete: serenity::model::guild::UnavailableGuild,
        _full: Option<Guild>,
    ) {
        self.cache.remove_guild_data(&incomplete.id.to_string());
    }

    async fn guild_member_update(
        &self,
        _ctx: Context,
        _old_if_available: Option<Member>,
        new: Option<Member>,
        _event: serenity::model::event::GuildMemberUpdateEvent,
    ) {
        let Some(member) = new else { return };

        let discriminator = Self::get_discriminator(&member.user);
        let avatar = member.user.face();

        tracing::info!("Member profile updated: {}", member.user.name);

        if let Err(e) = self
            .sync_profile(
                member.user.id,
                &member.user.name,
                discriminator,
                &avatar,
                None,
            )
            .await
        {
            tracing::error!(
                "Failed to sync profile update for {}: {}",
                member.user.id,
                e
            );
        }
    }

    async fn message(&self, ctx: Context, message: Message) {
        // Ignore bot messages
        if message.author.bot {
            return;
        }

        // Ignore messages from our own webhooks to prevent loops
        if self.is_bridge_webhook(&message) {
            tracing::debug!("Ignoring message from bridge webhook");
            return;
        }

        let channel_id_str = message.channel_id.to_string();

        // Check if channel is bridged
        let channels = match self.db.list_channels().await {
            Ok(ch) => ch,
            Err(e) => {
                tracing::error!("Failed to list channels: {}", e);
                return;
            }
        };

        if !channels.contains(&channel_id_str) {
            return;
        }

        // Resolve Matrix room ID
        let Some(bridge) = self.resolve_bridge(&channel_id_str).await else {
            tracing::warn!(
                "Could not resolve Matrix room for Discord channel {}",
                channel_id_str
            );
            return;
        };
        if !bridge.d2m_enabled {
            return;
        }
        let room_id = bridge.room_id;

        // Ensure user exists and is in room
        let mxid = match self.ensure_user_in_room(&ctx, &message, &room_id).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Failed to ensure user in room: {}", e);
                return;
            }
        };

        // Process message content
        let (mut content, emotes) = self.process_discord_message(&message);

        // Add embeds to content
        let embed_text = Self::process_embeds(&message);
        if !embed_text.is_empty() {
            content.push_str(&embed_text);
        }

        // Check if this is a reply
        let reply_to_event_id = message.referenced_message.as_ref().and_then(|referenced| {
            // Look up the Matrix event ID for the referenced Discord message
            self.cache.d_messages.get(&referenced.id.to_string())
        });

        // Send text message if there's content
        let mut message_event_id = None;
        if !content.trim().is_empty() {
            let msg_content = self
                .create_matrix_message_content_with_emojis(
                    &room_id,
                    &content,
                    &emotes,
                    reply_to_event_id,
                )
                .await;

            match self
                .matrix
                .send_message(&room_id, msg_content, Some(&mxid))
                .await
            {
                Ok(event_id) => {
                    tracing::info!(
                        "Forwarded Discord message {} to Matrix event {}",
                        message.id,
                        event_id
                    );
                    message_event_id = Some(event_id.clone());

                    self.cache
                        .insert_message_mapping(event_id, message.id.to_string());
                }
                Err(e) => {
                    tracing::error!("Failed to send message to Matrix room {}: {}", room_id, e);
                }
            }
        }

        // Send attachments as separate messages
        let attachments = Self::process_attachments(&message);
        if !attachments.is_empty() {
            match self
                .send_attachments_to_matrix(&room_id, &mxid, attachments)
                .await
            {
                Ok(event_ids) => {
                    // Cache first attachment event ID if we don't have a text message
                    if message_event_id.is_none() && !event_ids.is_empty() {
                        self.cache
                            .insert_message_mapping(event_ids[0].clone(), message.id.to_string());
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to send attachments: {}", e);
                }
            }
        }

        // Send stickers
        let stickers = Self::process_stickers(&message);
        for sticker_url in stickers {
            let sticker_name = sticker_url.split('/').next_back().unwrap_or("sticker.png");

            match self
                .matrix
                .send_sticker(&room_id, &mxid, &sticker_url, sticker_name)
                .await
            {
                Ok(event_id) => {
                    tracing::info!(
                        "Forwarded Discord message {} to Matrix event {}",
                        message.id,
                        event_id
                    );

                    // Move event_id here — this is the one and only move
                    message_event_id = Some(event_id);

                    // Now borrow from message_event_id instead of owning again
                    if let Some(ref eid) = message_event_id {
                        self.cache
                            .insert_message_mapping(eid.clone(), message.id.to_string());
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to send sticker to Matrix: {}", e);
                }
            }
        }
    }

    async fn message_update(
        &self,
        _ctx: Context,
        _old_if_available: Option<Message>,
        new: Option<Message>,
        _event: MessageUpdateEvent,
    ) {
        // We need the new message content
        let Some(new_msg) = new else {
            tracing::debug!("Message update without new content, ignoring");
            return;
        };

        // Ignore bot messages
        if new_msg.author.bot {
            return;
        }

        // Ignore our own webhooks
        if self.is_bridge_webhook(&new_msg) {
            return;
        }

        let channel_id_str = new_msg.channel_id.to_string();

        // Check if channel is bridged
        let channels = match self.db.list_channels().await {
            Ok(ch) => ch,
            Err(e) => {
                tracing::error!("Failed to list channels: {}", e);
                return;
            }
        };

        if !channels.contains(&channel_id_str) {
            return;
        }

        let matrix_event_id = self.cache.d_messages.get(&new_msg.id.to_string());

        let Some(matrix_event_id) = matrix_event_id else {
            tracing::debug!(
                "No Matrix event found for Discord message edit {}",
                new_msg.id
            );
            return;
        };

        // Resolve room ID
        let Some(bridge) = self.resolve_bridge(&channel_id_str).await else {
            tracing::warn!("Could not resolve Matrix room for edit");
            return;
        };
        if !bridge.d2m_enabled {
            return;
        }
        let room_id = bridge.room_id;

        let mxid = self
            .matrix
            .matrixify_user(&new_msg.author.id.to_string(), None);

        // Process the new content
        let (mut content, emotes) = self.process_discord_message(&new_msg);

        // Add embeds
        let embed_text = Self::process_embeds(&new_msg);
        if !embed_text.is_empty() {
            content.push_str(&embed_text);
        }

        let (plain_body, formatted_body) = MatrixClient::process_for_matrix(&content, &emotes);

        let new_content = if formatted_body == plain_body {
            RoomMessageEventContent::text_plain(plain_body)
        } else {
            RoomMessageEventContent::text_html(plain_body, formatted_body)
        };

        match self
            .matrix
            .send_edit(&room_id, &matrix_event_id, new_content, Some(&mxid))
            .await
        {
            Ok(new_event_id) => {
                tracing::info!(
                    "Sent Matrix edit {} for Discord message {}",
                    new_event_id,
                    new_msg.id
                );
            }
            Err(e) => {
                tracing::error!("Failed to send edit to Matrix: {}", e);
            }
        }
    }

    async fn message_delete(
        &self,
        _ctx: Context,
        channel_id: ChannelId,
        deleted_message_id: MessageId,
        _guild_id: Option<GuildId>,
    ) {
        let channel_id_str = channel_id.to_string();

        // Check if channel is bridged
        let channels = match self.db.list_channels().await {
            Ok(ch) => ch,
            Err(e) => {
                tracing::error!("Failed to list channels: {}", e);
                return;
            }
        };

        if !channels.contains(&channel_id_str) {
            return;
        }

        let matrix_event_id = self.cache.d_messages.get(&deleted_message_id.to_string());

        let Some(matrix_event_id) = matrix_event_id else {
            tracing::debug!(
                "No Matrix event found for Discord message deletion {}",
                deleted_message_id
            );
            return;
        };

        // Resolve room ID
        let Some(bridge) = self.resolve_bridge(&channel_id_str).await else {
            tracing::warn!("Could not resolve Matrix room for deletion");
            return;
        };

        if !bridge.d2m_enabled {
            return;
        }

        let is_webhook_message = match self
            .matrix
            .get_event(&bridge.room_id, &matrix_event_id)
            .await
        {
            Ok(ev) => !ev.sender.starts_with("@_discord_"),
            Err(_) => true,
        };

        if is_webhook_message && !bridge.d2m_mod_deletions {
            tracing::debug!("Ignoring D->M mod deletion");
            return;
        }

        let room_id = bridge.room_id;

        // Send redaction to Matrix
        match self
            .matrix
            .redact_event(&room_id, &matrix_event_id, None)
            .await
        {
            Ok(()) => {
                tracing::info!(
                    "Sent Matrix redaction for Discord message deletion {}",
                    deleted_message_id
                );

                self.cache.remove_message_mapping(
                    Some(&matrix_event_id),
                    Some(&deleted_message_id.to_string()),
                );
            }
            Err(e) => {
                tracing::error!("Failed to redact Matrix event {}: {}", matrix_event_id, e);
            }
        }
    }

    async fn reaction_add(&self, ctx: Context, reaction: Reaction) {
        let channel_id_str = reaction.channel_id.to_string();

        let channels = match self.db.list_channels().await {
            Ok(ch) => ch,
            Err(e) => {
                tracing::error!("Failed to list channels: {}", e);
                return;
            }
        };

        if !channels.contains(&channel_id_str) {
            return;
        }

        let matrix_event_id = self.cache.d_messages.get(&reaction.message_id.to_string());

        let Some(matrix_event_id) = matrix_event_id else {
            tracing::debug!(
                "No Matrix event found for Discord reaction on message {}",
                reaction.message_id
            );
            return;
        };

        let Some(bridge) = self.resolve_bridge(&channel_id_str).await else {
            tracing::warn!("Could not resolve Matrix room for reaction");
            return;
        };
        if !bridge.d2m_enabled {
            return;
        }
        let room_id = bridge.room_id;

        // Get the user who reacted
        let user = if let Some(uid) = reaction.user_id {
            match ctx.http.get_user(uid).await {
                Ok(u) => u,
                Err(e) => {
                    tracing::error!("Failed to get user {}: {}", uid, e);
                    return;
                }
            }
        } else {
            tracing::warn!("Reaction without user_id");
            return;
        };

        // Don't react for bots
        if user.bot {
            return;
        }

        let mxid = self.matrix.matrixify_user(&user.id.to_string(), None);

        let reaction_name = match &reaction.emoji {
            ReactionType::Unicode(emoji) => emoji.clone(),
            ReactionType::Custom { name, id, .. } => name
                .as_ref()
                .map_or_else(|| format!("custom_{id}"), std::clone::Clone::clone),
            _ => {
                tracing::debug!("Unknown reaction type");
                return;
            }
        };

        let matrix_reaction_key = match &reaction.emoji {
            ReactionType::Unicode(emoji) => emoji.clone(),
            ReactionType::Custom { animated, id, name } => {
                let emote_name = name
                    .as_ref()
                    .map_or_else(|| format!("custom_{id}"), std::clone::Clone::clone);

                let mut mxc = if let Ok(room_emojis) = self.matrix.get_room_emojis(&room_id).await
                    && let Some(url) = room_emojis.get(&emote_name)
                {
                    Some(url.clone())
                } else {
                    None
                };

                if mxc.is_none()
                    && let Some(url) = self.cache.m_emotes.get(&emote_name)
                {
                    mxc = Some(url);
                }

                if mxc.is_none() {
                    let ext = if *animated { "gif" } else { "png" };
                    let discord_url = format!("https://cdn.discordapp.com/emojis/{id}.{ext}");

                    if let Ok(url) = self.matrix.upload_from_url(&discord_url).await {
                        self.cache.m_emotes.insert(emote_name.clone(), url.clone());
                        mxc = Some(url);
                    } else {
                        tracing::warn!(
                            "Failed to upload Discord emoji for reaction: {}",
                            emote_name
                        );
                    }
                }

                mxc.unwrap_or(emote_name)
            }
            _ => return,
        };

        match self
            .matrix
            .send_reaction(&room_id, &matrix_event_id, &matrix_reaction_key, &mxid)
            .await
        {
            Ok(reaction_event_id) => {
                tracing::info!(
                    "Sent Matrix reaction {} for Discord reaction",
                    reaction_event_id
                );

                let cache_key = format!("{}:{}:{}", reaction.message_id, user.id, reaction_name);
                self.cache.d_messages.insert(cache_key, reaction_event_id);
            }
            Err(e) => {
                tracing::error!("Failed to send reaction to Matrix: {}", e);
            }
        }
    }

    async fn reaction_remove(&self, ctx: Context, reaction: Reaction) {
        let channel_id_str = reaction.channel_id.to_string();

        let channels = match self.db.list_channels().await {
            Ok(ch) => ch,
            Err(e) => {
                tracing::error!("Failed to list channels: {}", e);
                return;
            }
        };

        if !channels.contains(&channel_id_str) {
            return;
        }

        let Some(bridge) = self.resolve_bridge(&channel_id_str).await else {
            tracing::warn!("Could not resolve Matrix room for reaction removal");
            return;
        };
        if !bridge.d2m_enabled {
            return;
        }
        let room_id = bridge.room_id;

        let user = if let Some(uid) = reaction.user_id {
            match ctx.http.get_user(uid).await {
                Ok(u) => u,
                Err(e) => {
                    tracing::error!("Failed to get user {}: {}", uid, e);
                    return;
                }
            }
        } else {
            tracing::warn!("Reaction removal without user_id");
            return;
        };

        if user.bot {
            return;
        }

        let reaction_key = match &reaction.emoji {
            ReactionType::Unicode(emoji) => emoji.clone(),
            ReactionType::Custom { name, id, .. } => name
                .as_ref()
                .map_or_else(|| format!("custom_{id}"), std::clone::Clone::clone),
            _ => return,
        };

        // Find the reaction event ID from cache
        let cache_key = format!("{}:{}:{}", reaction.message_id, user.id, reaction_key);
        let reaction_event_id = self.cache.d_messages.get(&cache_key);

        if let Some(event_id) = reaction_event_id {
            match self.matrix.redact_event(&room_id, &event_id, None).await {
                Ok(()) => {
                    tracing::info!("Redacted Matrix reaction {}", event_id);
                    self.cache.d_messages.invalidate(&cache_key);
                }
                Err(e) => {
                    tracing::error!("Failed to redact reaction: {}", e);
                }
            }
        } else {
            tracing::debug!("No Matrix reaction event found for removal");
        }
    }

    async fn typing_start(&self, _ctx: Context, typing: serenity::model::event::TypingStartEvent) {
        let channel_id_str = typing.channel_id.to_string();

        // Check if channel is bridged
        let channels = match self.db.list_channels().await {
            Ok(ch) => ch,
            Err(e) => {
                tracing::error!("Failed to list channels: {}", e);
                return;
            }
        };

        if !channels.contains(&channel_id_str) {
            return;
        }

        // Resolve room ID
        let Some(bridge) = self.resolve_bridge(&channel_id_str).await else {
            return;
        };
        if !bridge.d2m_enabled || !bridge.d2m_typing {
            return;
        }
        let room_id = bridge.room_id;

        let mxid = self
            .matrix
            .matrixify_user(&typing.user_id.to_string(), None);

        // Send typing notification
        if let Err(e) = self.matrix.send_typing(&room_id, &mxid, 8000).await {
            tracing::debug!("Failed to send typing notification: {}", e);
        }
    }
}

pub async fn create_discord_client(
    token: String,
    matrix: Arc<MatrixClient>,
    db: Database,
    cache: Cache,
    config: Config,
) -> anyhow::Result<Client> {
    let intents = serenity::model::gateway::GatewayIntents::GUILDS
        | serenity::model::gateway::GatewayIntents::GUILD_MESSAGES
        | serenity::model::gateway::GatewayIntents::GUILD_MEMBERS
        | serenity::model::gateway::GatewayIntents::MESSAGE_CONTENT
        | serenity::model::gateway::GatewayIntents::GUILD_EMOJIS_AND_STICKERS
        | serenity::model::gateway::GatewayIntents::GUILD_MESSAGE_TYPING
        | serenity::model::gateway::GatewayIntents::GUILD_MESSAGE_REACTIONS;

    let handler = DiscordHandler::new(matrix, db, cache, config);

    let client = Client::builder(&token, intents)
        .event_handler(handler)
        .await?;

    Ok(client)
}
