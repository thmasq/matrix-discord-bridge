use crate::{cache::Cache, config::Config, db::Database, matrix_client::MatrixClient};
use ruma::events::room::message::RoomMessageEventContent;
use secrecy::ExposeSecret;
use serenity::{
    Client,
    all::{
        ChannelId, CreateWebhook, GuildId, MessageId, StickerFormatType, UserId as DiscordUserId,
    },
    async_trait,
    client::{Context, EventHandler},
    model::{
        channel::Message, event::MessageUpdateEvent, gateway::Ready, guild::Guild, id::WebhookId,
    },
};
use std::{collections::HashMap, sync::Arc};

pub struct DiscordHandler {
    matrix: Arc<MatrixClient>,
    db: Database,
    cache: Cache,
    config: Config,
}

impl DiscordHandler {
    pub fn new(matrix: Arc<MatrixClient>, db: Database, cache: Cache, config: Config) -> Self {
        Self {
            matrix,
            db,
            cache,
            config,
        }
    }

    fn get_discriminator(user: &serenity::model::user::User) -> u16 {
        user.discriminator
            .map(|d| d.get()) // Extract u16 from NonZero<u16>
            .unwrap_or(0) // Default to 0 for new system
    }

    async fn get_or_create_webhook(
        &self,
        ctx: &Context,
        channel_id: ChannelId,
    ) -> Option<(WebhookId, String)> {
        // Check cache first
        {
            let webhooks = self.cache.d_webhooks.read();
            if let Some(info) = webhooks.get(&channel_id.to_string()) {
                if let Ok(id) = info.id.parse::<u64>() {
                    return Some((WebhookId::new(id), info.token.clone()));
                }
            }
        }

        // Fetch existing webhooks for this channel
        let webhooks = match channel_id.webhooks(&ctx.http).await {
            Ok(hooks) => hooks,
            Err(e) => {
                tracing::error!("Failed to fetch webhooks for channel {}: {}", channel_id, e);
                return None;
            }
        };

        // Find or create our bridge webhook
        let webhook = webhooks
            .iter()
            .find(|w| w.name.as_deref() == Some("matrix_bridge"))
            .cloned();

        let webhook = if let Some(wh) = webhook {
            wh
        } else {
            match channel_id
                .create_webhook(&ctx.http, CreateWebhook::new("matrix_bridge"))
                .await
            {
                Ok(wh) => wh,
                Err(e) => {
                    tracing::error!("Failed to create webhook for channel {}: {}", channel_id, e);
                    return None;
                }
            }
        };

        let token = webhook.token.clone()?;
        let token_string = token.expose_secret().clone();

        // Cache it
        {
            let mut webhooks = self.cache.d_webhooks.write();
            webhooks.insert(
                channel_id.to_string(),
                crate::cache::WebhookInfo {
                    id: webhook.id.to_string(),
                    token: token_string.clone(),
                },
            );
        }

        Some((webhook.id, token_string))
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

        // Process channel mentions: <#123456> -> #channel-name
        let channel_regex = regex::Regex::new(r"<#(\d+)>").unwrap();
        for cap in channel_regex.captures_iter(&message.content.clone()) {
            if let Some(channel_id_match) = cap.get(1) {
                let channel_id = channel_id_match.as_str();
                // Try to resolve channel name
                if let Some(_guild_id) = message.guild_id {
                    // In a production system, we'd cache channel names
                    // For now, use a generic format
                    content = content.replace(&cap[0], &format!("#channel-{}", channel_id));
                } else {
                    content = content.replace(&cap[0], "#deleted-channel");
                }
            }
        }

        // Process emotes: <:name:id> or <a:name:id>
        let emote_regex = regex::Regex::new(r"<a?:(\w+):(\d+)>").unwrap();
        for cap in emote_regex.captures_iter(&message.content) {
            let name = cap.get(1).unwrap().as_str();
            let id = cap.get(2).unwrap().as_str();
            emotes.insert(name.to_string(), id.to_string());
        }
        content = emote_regex.replace_all(&content, ":$1:").to_string();

        // Append attachments
        for attachment in &message.attachments {
            content.push_str(&format!("\n{}", attachment.url));
        }

        // Append stickers (skip Lottie format type 3)
        for sticker in &message.sticker_items {
            if sticker.format_type != StickerFormatType::Lottie {
                content.push_str(&format!(
                    "\nhttps://cdn.discordapp.com/stickers/{}.png",
                    sticker.id
                ));
            }
        }

        (content, emotes)
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
        let profile = match self.db.fetch_user(&mxid).await? {
            Some(p) => p,
            None => {
                // User doesn't exist yet, will be created on first message
                return Ok(());
            }
        };

        let display_name = if discriminator == 0 {
            // New username system (no discriminator)
            username.to_string()
        } else {
            // Legacy username system
            format!("{}#{:04}", username, discriminator)
        };

        let mut updated = false;

        // Update display name if changed
        if profile.username.as_deref() != Some(&display_name) {
            match self.matrix.set_display_name(&mxid, &display_name).await {
                Ok(_) => {
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
                Ok(_) => {
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
                crate::utils::hash_string(&message.author.name)
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

            if let Some(avatar) = message.author.avatar_url() {
                if let Err(e) = self.matrix.set_avatar(&mxid, &avatar).await {
                    tracing::warn!("Failed to set avatar for {}: {}", mxid, e);
                }
            }
        } else if message.webhook_id.is_some() {
            // For webhook messages, always sync profile
            let discriminator = Self::get_discriminator(&message.author);
            let avatar = message.author.avatar_url().unwrap_or_default();
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

        // Ensure user is in the room by inviting and joining
        // Check if already in room (simplified - in production, query room members)
        if let Err(e) = self.matrix.send_invite(room_id, &mxid).await {
            tracing::debug!("Invite failed for {} (may already be in room): {}", mxid, e);
        }

        if let Err(e) = self.matrix.join_room(room_id, Some(&mxid)).await {
            tracing::debug!("Join failed for {} (may already be in room): {}", mxid, e);
        }

        Ok(mxid)
    }

    async fn resolve_room_id(&self, channel_id: &str) -> Option<String> {
        let room_alias = self.matrix.matrixify_room(channel_id);

        // Check cache first
        {
            let rooms = self.cache.m_rooms.read();
            if let Some(room_id) = rooms.get(&room_alias) {
                return Some(room_id.clone());
            }
        }

        // Try to resolve via Matrix API
        match self.matrix.resolve_room_alias(&room_alias).await {
            Ok(room_id) => {
                // Cache it
                self.cache
                    .m_rooms
                    .write()
                    .insert(room_alias, room_id.clone());
                Some(room_id)
            }
            Err(e) => {
                tracing::debug!("Could not resolve room alias {}: {}", room_alias, e);
                None
            }
        }
    }

    fn is_bridge_webhook(&self, message: &Message) -> bool {
        if let Some(webhook_id) = message.webhook_id {
            let webhooks = self.cache.d_webhooks.read();
            for info in webhooks.values() {
                if info.id == webhook_id.to_string() {
                    return true;
                }
            }
        }
        false
    }

    async fn create_matrix_message_content(
        &self,
        content: &str,
        emotes: &HashMap<String, String>,
        reply_to_event_id: Option<String>,
    ) -> RoomMessageEventContent {
        // Process markdown and emotes
        let (plain_body, formatted_body) = self.matrix.process_for_matrix(content, emotes).await;

        let event_content = if formatted_body != plain_body {
            RoomMessageEventContent::text_html(plain_body.clone(), formatted_body)
        } else {
            RoomMessageEventContent::text_plain(plain_body.clone())
        };

        // Add reply metadata if this is a reply
        if let Some(event_id) = reply_to_event_id {
            // In a full implementation, you'd set the m.relates_to field
            // For now, we'll just send the message normally
            // The proper way would be to construct a reply with:
            // - m.relates_to with m.in_reply_to
            // - Fallback quote in body
            tracing::debug!("Reply to event {} (reply handling simplified)", event_id);
        }

        event_content
    }
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        tracing::info!("Discord bot connected as {}", ready.user.name);
    }

    async fn guild_create(&self, _ctx: Context, guild: Guild, _is_new: Option<bool>) {
        tracing::info!("Guild available: {} ({})", guild.name, guild.id);

        // Cache emotes
        {
            let mut emotes = self.cache.d_emotes.write();
            for (emoji_id, emoji) in &guild.emojis {
                let emote_str = if emoji.animated {
                    format!("<a:{}:{}>", emoji.name, emoji_id)
                } else {
                    format!("<:{}:{}>", emoji.name, emoji_id)
                };
                emotes.insert(emoji.name.clone(), emote_str);
            }
            tracing::debug!(
                "Cached {} emotes from guild {}",
                guild.emojis.len(),
                guild.id
            );
        }

        // Sync profiles for all members (async in background)
        let member_count = guild.members.len();
        tracing::info!(
            "Syncing profiles for {} members in guild {}",
            member_count,
            guild.id
        );

        for (user_id, member) in guild.members {
            let discriminator = Self::get_discriminator(&member.user);

            let avatar = member.user.avatar_url().unwrap_or_default();

            if let Err(e) = self
                .sync_profile(user_id, &member.user.name, discriminator, &avatar, None)
                .await
            {
                tracing::debug!("Failed to sync profile for {}: {}", user_id, e);
            }
        }

        tracing::info!("Completed profile sync for guild {}", guild.id);
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
        let room_id = match self.resolve_room_id(&channel_id_str).await {
            Some(rid) => rid,
            None => {
                tracing::warn!(
                    "Could not resolve Matrix room for Discord channel {}",
                    channel_id_str
                );
                return;
            }
        };

        // Ensure user exists and is in room
        let mxid = match self.ensure_user_in_room(&ctx, &message, &room_id).await {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("Failed to ensure user in room: {}", e);
                return;
            }
        };

        // Process message content
        let (content, emotes) = self.process_discord_message(&message);

        // Check if this is a reply
        let reply_to_event_id = if let Some(ref referenced) = message.referenced_message {
            // Look up the Matrix event ID for the referenced Discord message
            let d_messages = self.cache.d_messages.read();
            d_messages.get(&referenced.id.to_string()).cloned()
        } else {
            None
        };

        // Create Matrix message content
        let msg_content = self
            .create_matrix_message_content(&content, &emotes, reply_to_event_id)
            .await;

        // Send to Matrix
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

                // Cache the mapping
                self.cache
                    .d_messages
                    .write()
                    .insert(message.id.to_string(), event_id.clone());
                self.cache
                    .m_messages
                    .write()
                    .insert(event_id, message.id.to_string());
            }
            Err(e) => {
                tracing::error!("Failed to send message to Matrix room {}: {}", room_id, e);
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
        let new_msg = match new {
            Some(msg) => msg,
            None => {
                tracing::debug!("Message update without new content, ignoring");
                return;
            }
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

        // Find the original Matrix event ID
        let matrix_event_id = {
            let d_messages = self.cache.d_messages.read();
            d_messages.get(&new_msg.id.to_string()).cloned()
        };

        let _matrix_event_id = match matrix_event_id {
            Some(id) => id,
            None => {
                tracing::debug!(
                    "No Matrix event found for Discord message edit {}",
                    new_msg.id
                );
                return;
            }
        };

        // Resolve room ID
        let room_id = match self.resolve_room_id(&channel_id_str).await {
            Some(rid) => rid,
            None => {
                tracing::warn!("Could not resolve Matrix room for edit");
                return;
            }
        };

        let mxid = self
            .matrix
            .matrixify_user(&new_msg.author.id.to_string(), None);

        // Process the new content
        let (content, emotes) = self.process_discord_message(&new_msg);
        let (plain_body, formatted_body) = self.matrix.process_for_matrix(&content, &emotes).await;

        // Create edit event content
        // This creates a proper m.replace event
        let _new_content = if formatted_body != plain_body {
            RoomMessageEventContent::text_html(plain_body.clone(), formatted_body.clone())
        } else {
            RoomMessageEventContent::text_plain(plain_body.clone())
        };

        // Create the replacement event
        // In ruma, we need to manually construct the edit structure
        // For now, send a new message indicating it's an edit
        let edit_content = RoomMessageEventContent::text_plain(format!("[edit] {}", plain_body));

        match self
            .matrix
            .send_message(&room_id, edit_content, Some(&mxid))
            .await
        {
            Ok(_new_event_id) => {
                tracing::info!(
                    "Forwarded Discord edit {} to Matrix (simplified edit)",
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

        // Find the Matrix event ID for this Discord message
        let matrix_event_id = {
            let d_messages = self.cache.d_messages.read();
            d_messages.get(&deleted_message_id.to_string()).cloned()
        };

        let matrix_event_id = match matrix_event_id {
            Some(id) => id,
            None => {
                tracing::debug!(
                    "No Matrix event found for Discord message deletion {}",
                    deleted_message_id
                );
                return;
            }
        };

        // Resolve room ID
        let room_id = match self.resolve_room_id(&channel_id_str).await {
            Some(rid) => rid,
            None => {
                tracing::warn!("Could not resolve Matrix room for deletion");
                return;
            }
        };

        // Send redaction to Matrix
        match self
            .matrix
            .redact_event(&room_id, &matrix_event_id, None)
            .await
        {
            Ok(_) => {
                tracing::info!(
                    "Sent Matrix redaction for Discord message deletion {}",
                    deleted_message_id
                );

                // Clean up cache
                self.cache
                    .d_messages
                    .write()
                    .remove(&deleted_message_id.to_string());
                self.cache.m_messages.write().remove(&matrix_event_id);
            }
            Err(e) => {
                tracing::error!("Failed to redact Matrix event {}: {}", matrix_event_id, e);
            }
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
        let room_id = match self.resolve_room_id(&channel_id_str).await {
            Some(rid) => rid,
            None => return,
        };

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
        | serenity::model::gateway::GatewayIntents::GUILD_MESSAGE_TYPING;

    let handler = DiscordHandler::new(matrix, db, cache, config);

    let client = Client::builder(&token, intents)
        .event_handler(handler)
        .await?;

    Ok(client)
}
