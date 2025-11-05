use crate::{cache::Cache, config::Config, db::Database, matrix_client::MatrixClient};
use secrecy::ExposeSecret;
use serenity::{
    Client,
    all::{ChannelId, CreateWebhook, GuildId, MessageId, UserId as DiscordUserId},
    async_trait,
    client::{Context, EventHandler},
    model::{channel::Message, gateway::Ready, guild::Guild, id::WebhookId},
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

    async fn get_or_create_webhook(
        &self,
        ctx: &Context,
        channel_id: ChannelId,
    ) -> Option<(WebhookId, String)> {
        // Check cache
        {
            let webhooks = self.cache.d_webhooks.read();
            if let Some(info) = webhooks.get(&channel_id.to_string()) {
                if let Ok(id) = info.id.parse::<u64>() {
                    return Some((WebhookId::new(id), info.token.clone()));
                }
            }
        }

        // Fetch or create webhook
        let webhooks = channel_id.webhooks(&ctx.http).await.ok()?;
        let webhook = webhooks
            .iter()
            .find(|w| w.name.as_deref() == Some("matrix_bridge"))
            .cloned();

        let webhook = if let Some(wh) = webhook {
            wh
        } else {
            channel_id
                .create_webhook(&ctx.http, CreateWebhook::new("matrix_bridge"))
                .await
                .ok()?
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

        // Process mentions
        for user in &message.mentions {
            let mention = format!("<@{}>", user.id);
            let mention_nick = format!("<@!{}>", user.id);
            content = content.replace(&mention, &format!("@{}", user.name));
            content = content.replace(&mention_nick, &format!("@{}", user.name));
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

        (content, emotes)
    }

    async fn sync_profile(
        &self,
        user_id: DiscordUserId,
        username: &str,
        discriminator: &str,
        avatar_url: &str,
        hashed: Option<&str>,
    ) {
        let mxid = self.matrix.matrixify_user(&user_id.to_string(), hashed);

        if let Ok(Some(profile)) = self.db.fetch_user(&mxid).await {
            let display_name = format!("{}#{}", username, discriminator);

            if profile.username.as_deref() != Some(&display_name) {
                let _ = self.matrix.set_display_name(&mxid, &display_name).await;
            }

            if profile.avatar_url.as_deref() != Some(avatar_url) {
                let _ = self.matrix.set_avatar(&mxid, avatar_url).await;
            }
        }
    }
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        tracing::info!("Discord bot connected as {}", ready.user.name);
    }

    async fn guild_create(&self, _ctx: Context, guild: Guild, _is_new: Option<bool>) {
        // Cache emotes
        let mut emotes = self.cache.d_emotes.write();
        for emoji in &guild.emojis {
            let emote_str = if emoji.1.animated {
                format!("<a:{}:{}>", emoji.1.name, emoji.0)
            } else {
                format!("<:{}:{}>", emoji.1.name, emoji.0)
            };
            emotes.insert(emoji.1.name.clone(), emote_str);
        }
    }

    async fn message(&self, ctx: Context, message: Message) {
        // Ignore bot messages and messages from our webhooks
        if message.author.bot {
            return;
        }

        let channel_id_str = message.channel_id.to_string();

        // Check if channel is bridged
        let channels = self.db.list_channels().await.unwrap_or_default();
        if !channels.contains(&channel_id_str) {
            return;
        }

        // Get Matrix room
        let room_alias = self.matrix.matrixify_room(&channel_id_str);
        // In a full implementation, you'd resolve the alias to room_id

        // Process message
        let (content, emotes) = self.process_discord_message(&message);

        // Get or create puppet user
        let mxid = self
            .matrix
            .matrixify_user(&message.author.id.to_string(), None);

        if self.db.fetch_user(&mxid).await.unwrap_or(None).is_none() {
            let _ = self.matrix.register_user(&mxid).await;
            let display_name =
                format!("{}#{:?}", message.author.name, message.author.discriminator);
            let _ = self.matrix.set_display_name(&mxid, &display_name).await;

            if let Some(avatar) = message.author.avatar_url() {
                let _ = self.matrix.set_avatar(&mxid, &avatar).await;
            }
        }

        // Send to Matrix
        let msg_content = ruma::events::room::message::RoomMessageEventContent::text_plain(content);
        // In full implementation, send the message and cache the event_id
    }

    async fn message_update(
        &self,
        ctx: Context,
        old_if_available: Option<Message>,
        new: Option<Message>,
        event: serenity::model::event::MessageUpdateEvent,
    ) {
        // Handle message edits
        if let Some(new_msg) = new {
            // Similar to message handling but with edit logic
        }
    }

    async fn message_delete(
        &self,
        ctx: Context,
        channel_id: ChannelId,
        deleted_message_id: MessageId,
        guild_id: Option<GuildId>,
    ) {
        // Handle message deletions
        // Look up the Matrix event_id from cache and send a redaction
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
        | serenity::model::gateway::GatewayIntents::MESSAGE_CONTENT;

    let handler = DiscordHandler::new(matrix, db, cache, config);

    let client = Client::builder(&token, intents)
        .event_handler(handler)
        .await?;

    Ok(client)
}
