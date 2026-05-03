use crate::{
    admin_commands::AdminCommandHandler, cache::Cache, config::Config, db::Database,
    error::BridgeError, matrix_client::MatrixClient, utils::DISCORD_MESSAGE_LIMIT,
};
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::net::TcpListener;

pub struct AppService {
    config: Config,
    matrix: Arc<MatrixClient>,
    db: Database,
    cache: Cache,
    discord_http: reqwest::Client,
    admin_handler: AdminCommandHandler,
}

impl AppService {
    pub fn new(config: Config, matrix: Arc<MatrixClient>, db: Database, cache: Cache) -> Self {
        let discord_http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        let admin_handler = AdminCommandHandler::new(
            config.clone(),
            matrix.clone(),
            db.clone(),
            cache.clone(),
            discord_http.clone(),
        );

        Self {
            config,
            matrix,
            db,
            cache,
            discord_http,
            admin_handler,
        }
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let addr = format!("0.0.0.0:{}", self.config.port);
        let listener = TcpListener::bind(&addr).await?;
        tracing::info!("Appservice listening on {}", addr);

        loop {
            let (stream, _) = listener.accept().await?;
            let io = TokioIo::new(stream);
            let service = self.clone();

            tokio::spawn(async move {
                if let Err(e) = http1::Builder::new()
                    .serve_connection(
                        io,
                        service_fn(move |req| {
                            let svc = service.clone();
                            async move { svc.handle_request(req).await }
                        }),
                    )
                    .await
                {
                    tracing::error!("Error serving connection: {}", e);
                }
            });
        }
    }

    async fn handle_request(
        &self,
        req: Request<Incoming>,
    ) -> Result<Response<Full<Bytes>>, hyper::Error> {
        let response = match self.route_request(req).await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::error!("Request error: {}", e);
                let body = serde_json::json!({
                    "errcode": "M_UNKNOWN",
                    "error": e.to_string()
                });
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
                    .unwrap()
            }
        };

        Ok(response)
    }

    async fn route_request(
        &self,
        req: Request<Incoming>,
    ) -> crate::error::Result<Response<Full<Bytes>>> {
        let path = req.uri().path();
        let method = req.method();

        if method == Method::PUT && path.starts_with("/_matrix/app/v1/transactions/") {
            self.handle_transaction(req).await
        } else {
            Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Full::new(Bytes::from("{}")))
                .unwrap())
        }
    }

    async fn handle_transaction(
        &self,
        req: Request<Incoming>,
    ) -> crate::error::Result<Response<Full<Bytes>>> {
        // Verify hs_token
        let query = req.uri().query().unwrap_or("");
        let token = query
            .split('&')
            .find(|s| s.starts_with("access_token="))
            .and_then(|s| s.strip_prefix("access_token="));

        if token != Some(&self.config.hs_token) {
            return Ok(Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Full::new(Bytes::from(
                    serde_json::to_vec(&serde_json::json!({
                        "errcode": "APPSERVICE_FORBIDDEN"
                    }))
                    .unwrap(),
                )))
                .unwrap());
        }

        // Parse body
        let body = req.collect().await?.to_bytes();
        let json: Value = serde_json::from_slice(&body)?;
        let events = json["events"].as_array().unwrap();

        // Process events
        for event in events {
            if let Err(e) = self.handle_event(event).await {
                tracing::error!("Error handling event: {}", e);
            }
        }

        Ok(Response::builder()
            .status(StatusCode::OK)
            .body(Full::new(Bytes::from("{}")))
            .unwrap())
    }

    async fn handle_event(&self, event: &Value) -> crate::error::Result<()> {
        let event_type = event["type"].as_str().unwrap_or("");

        match event_type {
            "m.room.member" => self.handle_member_event(event).await,
            "m.room.message" => self.handle_message_event(event).await,
            "m.sticker" => self.handle_sticker_event(event).await,
            "m.room.redaction" => self.handle_redaction_event(event).await,
            "m.reaction" => self.handle_reaction_event(event).await,
            "m.typing" => self.handle_typing_event(event).await,
            "im.ponies.emote_rooms" | "im.ponies.room_emotes" => {
                self.handle_emoji_update(event).await
            }
            _ => {
                tracing::debug!("Unhandled event type: {}", event_type);
                Ok(())
            }
        }
    }

    async fn handle_member_event(&self, event: &Value) -> crate::error::Result<()> {
        let room_id = event["room_id"].as_str().unwrap();
        let _sender = event["sender"].as_str().unwrap();
        let state_key = event["state_key"].as_str().unwrap();
        let content = &event["content"];

        // Clear member cache for this room
        self.cache.m_members.write().remove(room_id);

        // If it's an invite to our bot user in a DM, join
        if state_key == self.config.full_user_id()
            && let Some(true) = content["is_direct"].as_bool()
        {
            tracing::info!("Ignoring invite from user");
            // tracing::info!("Joining DM room {}", room_id);
            // self.matrix.join_room(room_id, None).await?;
        }

        Ok(())
    }

    async fn handle_message_event(&self, event: &Value) -> crate::error::Result<()> {
        let room_id = event["room_id"].as_str().unwrap();
        let sender = event["sender"].as_str().unwrap();
        let event_id = event["event_id"].as_str().unwrap();
        let content = &event["content"];

        // Ignore our own messages (bot and puppet users)
        if sender.starts_with("@_discord_") || sender == self.config.full_user_id() {
            return Ok(());
        }

        let msgtype = content["msgtype"].as_str().unwrap_or("m.text");

        // Handle admin commands in config room
        if msgtype == "m.text" {
            let body = content["body"].as_str().unwrap_or("");
            if body.starts_with('!') {
                return self
                    .admin_handler
                    .handle_command(room_id, sender, body)
                    .await;
            }
        }

        // Check if this is an edit
        let relates_to = content.get("m.relates_to");
        if let Some(rel) = relates_to
            && rel["rel_type"].as_str() == Some("m.replace")
        {
            let original_event_id = rel["event_id"].as_str().unwrap_or("");
            return self
                .handle_message_edit(room_id, sender, event_id, original_event_id, content)
                .await;
        }

        // Check if room is bridged
        if let Some(channel_id) = self.db.get_channel(room_id).await? {
            return self
                .forward_message_to_discord(
                    room_id,
                    sender,
                    event_id,
                    content,
                    &channel_id,
                    msgtype,
                )
                .await;
        }

        Ok(())
    }

    async fn handle_sticker_event(&self, event: &Value) -> crate::error::Result<()> {
        let room_id = event["room_id"].as_str().unwrap();
        let sender = event["sender"].as_str().unwrap();
        let event_id = event["event_id"].as_str().unwrap();
        let content = &event["content"];

        // Ignore our own stickers (bot and puppet users)
        if sender.starts_with("@_discord_") || sender == self.config.full_user_id() {
            return Ok(());
        }

        // Check if room is bridged
        let channel_id = match self.db.get_channel(room_id).await? {
            Some(id) => id,
            None => return Ok(()), // Not bridged
        };

        let url = content["url"]
            .as_str()
            .ok_or_else(|| BridgeError::Matrix("Sticker missing url field".to_string()))?;

        let body = content["body"].as_str().unwrap_or("sticker");

        // Download from Matrix
        let sticker_data = self.matrix.download_media(url).await?;

        // Get member info for display name and avatar
        let members = self.get_room_members(room_id).await?;
        let member = members.get(sender);

        let display_name = member.and_then(|m| m.display_name.as_ref()).map_or_else(
            || sender.split(':').next().unwrap_or(sender),
            std::string::String::as_str,
        );

        let avatar_url = member
            .and_then(|m| m.avatar_url.as_ref())
            .and_then(|mxc| self.matrix.mxc_to_http(mxc));

        tracing::info!(
            "Forwarding sticker from {} to Discord channel {}",
            sender,
            channel_id
        );

        // Get webhook
        let webhook = self.get_or_create_webhook(&channel_id).await?;

        // Determine filename from content or use default
        let info = content.get("info");
        let filename = if let Some(mimetype) = info.and_then(|i| i["mimetype"].as_str()) {
            if mimetype.contains("gif") {
                "sticker.gif"
            } else if mimetype.contains("webp") {
                "sticker.webp"
            } else {
                "sticker.png"
            }
        } else {
            "sticker.png"
        };

        // Send sticker as an image file to Discord
        let discord_msg_id = self
            .send_webhook_with_file(
                &webhook,
                body,
                display_name,
                avatar_url.as_deref(),
                filename,
                &sticker_data,
            )
            .await?;

        // Cache the mapping
        self.cache
            .m_messages
            .write()
            .insert(event_id.to_string(), discord_msg_id.clone());
        self.cache
            .d_messages
            .write()
            .insert(discord_msg_id, event_id.to_string());

        tracing::info!("Successfully sent Matrix sticker {} to Discord", event_id);

        Ok(())
    }

    async fn handle_emoji_update(&self, event: &Value) -> crate::error::Result<()> {
        let room_id = event["room_id"].as_str().unwrap();

        tracing::info!("Emoji pack updated in room {}, refreshing cache", room_id);

        // Clear the cache for this room
        self.cache.m_custom_emojis.write().remove(room_id);

        // Fetch fresh emoji data
        match self.matrix.fetch_room_emojis(room_id).await {
            Ok(emojis) => {
                tracing::info!("Cached {} custom emojis for room {}", emojis.len(), room_id);
            }
            Err(e) => {
                tracing::warn!("Failed to refresh emojis for room {}: {}", room_id, e);
            }
        }

        Ok(())
    }

    async fn forward_message_to_discord(
        &self,
        room_id: &str,
        sender: &str,
        event_id: &str,
        content: &Value,
        channel_id: &str,
        msgtype: &str,
    ) -> crate::error::Result<()> {
        match msgtype {
            "m.text" | "m.notice" | "m.emote" => {
                self.forward_text_to_discord(room_id, sender, event_id, content, channel_id)
                    .await
            }
            "m.image" | "m.file" | "m.video" | "m.audio" => {
                self.forward_media_to_discord(
                    room_id, sender, event_id, content, channel_id, msgtype,
                )
                .await
            }
            _ => {
                tracing::debug!("Unhandled message type: {}", msgtype);
                Ok(())
            }
        }
    }

    async fn forward_text_to_discord(
        &self,
        room_id: &str,
        sender: &str,
        event_id: &str,
        content: &Value,
        channel_id: &str,
    ) -> crate::error::Result<()> {
        let body = content["body"].as_str().unwrap_or("");
        let formatted_body = content["formatted_body"].as_str();

        // Parse custom emojis from the message
        let matrix_emojis = self.matrix.parse_matrix_emojis(body, formatted_body);

        // Get room's custom emojis and Discord emojis for matching
        let room_emojis = self
            .matrix
            .get_room_emojis(room_id)
            .await
            .unwrap_or_default();
        let discord_emojis = self.cache.d_emotes.read().clone();

        // Process the message for Discord
        let mut processed_body = body.to_string();

        // Handle Matrix mentions -> Discord mentions
        let text_to_process = formatted_body.unwrap_or(body);
        let mention_regex = regex::Regex::new(r"@_discord_(\d+)(?:-\d+)?:[\w.\-]+").unwrap();
        for cap in mention_regex.captures_iter(text_to_process) {
            if let Some(discord_id) = cap.get(1) {
                let mention = format!("<@{}>", discord_id.as_str());
                processed_body = processed_body.replace(&cap[0], &mention);
            }
        }

        // Handle Matrix custom emojis -> Discord emojis
        // Try to match by name first, otherwise fall back to uploading
        for (shortcode, mxc_url) in &matrix_emojis {
            let emoji_pattern = format!(":{shortcode}:");

            // Check if there's a Discord emoji with the same name
            if let Some(discord_format) = discord_emojis.get(shortcode) {
                // Found a matching Discord emoji by name - use it directly
                processed_body = processed_body.replace(&emoji_pattern, discord_format);
                tracing::debug!("Matched Matrix emoji :{}: to Discord emoji", shortcode);
            } else if !mxc_url.is_empty() {
                // No match found - this would require uploading the image
                // For now, just leave as :shortcode:
                // In a full implementation, you could download and upload to Discord
                tracing::debug!(
                    "No Discord match for Matrix emoji :{}: ({})",
                    shortcode,
                    mxc_url
                );
            } else if let Some(room_mxc) = room_emojis.get(shortcode) {
                // Found in room emojis but no Discord match
                tracing::debug!(
                    "Matrix emoji :{}: available in room ({}) but no Discord match",
                    shortcode,
                    room_mxc
                );
            }
        }

        // Fallback: Handle any remaining :name: patterns that might be Discord emojis
        {
            let emotes = self.cache.d_emotes.read();
            let emote_regex = regex::Regex::new(r":(\w+):").unwrap();
            for cap in emote_regex.captures_iter(body) {
                if let Some(name) = cap.get(1)
                    && let Some(discord_emote) = emotes.get(name.as_str())
                {
                    processed_body = processed_body.replace(&cap[0], discord_emote);
                }
            }
        }

        // Truncate to Discord's message limit
        if processed_body.len() > DISCORD_MESSAGE_LIMIT {
            processed_body.truncate(DISCORD_MESSAGE_LIMIT);
            processed_body.push('…');
        }

        // Get member info for avatar and display name
        let members = self.get_room_members(room_id).await?;
        let member = members.get(sender);

        let display_name = member.and_then(|m| m.display_name.as_ref()).map_or_else(
            || sender.split(':').next().unwrap_or(sender),
            std::string::String::as_str,
        );

        let avatar_url = member
            .and_then(|m| m.avatar_url.as_ref())
            .and_then(|mxc| self.matrix.mxc_to_http(mxc));

        tracing::info!(
            "Forwarding message from {} to Discord channel {}",
            sender,
            channel_id
        );

        // Get or create webhook
        let webhook = self.get_or_create_webhook(channel_id).await?;

        // Send via webhook
        let discord_msg_id = self
            .send_webhook_message(
                &webhook,
                &processed_body,
                display_name,
                avatar_url.as_deref(),
                None,
            )
            .await?;

        // Cache the mapping
        self.cache
            .m_messages
            .write()
            .insert(event_id.to_string(), discord_msg_id.clone());
        self.cache
            .d_messages
            .write()
            .insert(discord_msg_id, event_id.to_string());

        Ok(())
    }

    async fn forward_media_to_discord(
        &self,
        room_id: &str,
        sender: &str,
        event_id: &str,
        content: &Value,
        channel_id: &str,
        msgtype: &str,
    ) -> crate::error::Result<()> {
        let url = content["url"]
            .as_str()
            .ok_or_else(|| BridgeError::Matrix("Media message missing url field".to_string()))?;

        let body = content["body"].as_str().unwrap_or("attachment");

        // Download from Matrix
        let media_data = self.matrix.download_media(url).await?;

        // Get member info
        let members = self.get_room_members(room_id).await?;
        let member = members.get(sender);

        let display_name = member.and_then(|m| m.display_name.as_ref()).map_or_else(
            || sender.split(':').next().unwrap_or(sender),
            std::string::String::as_str,
        );

        let avatar_url = member
            .and_then(|m| m.avatar_url.as_ref())
            .and_then(|mxc| self.matrix.mxc_to_http(mxc));

        tracing::info!(
            "Forwarding {} from {} to Discord channel {}",
            msgtype,
            sender,
            channel_id
        );

        // Get webhook
        let webhook = self.get_or_create_webhook(channel_id).await?;

        // Upload to Discord and send
        let discord_msg_id = self
            .send_webhook_with_file(
                &webhook,
                body,
                display_name,
                avatar_url.as_deref(),
                body,
                &media_data,
            )
            .await?;

        // Cache the mapping
        self.cache
            .m_messages
            .write()
            .insert(event_id.to_string(), discord_msg_id.clone());
        self.cache
            .d_messages
            .write()
            .insert(discord_msg_id, event_id.to_string());

        Ok(())
    }

    async fn handle_message_edit(
        &self,
        room_id: &str,
        _sender: &str,
        new_event_id: &str,
        original_event_id: &str,
        content: &Value,
    ) -> crate::error::Result<()> {
        // Get the Discord message ID from the original event
        let discord_msg_id = {
            let messages = self.cache.m_messages.read();
            messages.get(original_event_id).cloned()
        };

        let discord_msg_id = if let Some(id) = discord_msg_id {
            id
        } else {
            tracing::debug!("No Discord message found for edit of {}", original_event_id);
            return Ok(());
        };

        // Get the new content
        let new_content = content
            .get("m.new_content")
            .ok_or_else(|| BridgeError::Matrix("Edit missing m.new_content".to_string()))?;

        let body = new_content["body"].as_str().unwrap_or("");

        // Process message similar to forward_to_discord
        let mut processed_body = body.to_string();

        // Handle mentions and emotes
        let formatted_body = new_content["formatted_body"].as_str();
        let text_to_process = formatted_body.unwrap_or(body);
        let mention_regex = regex::Regex::new(r"@_discord_(\d+)(?:-\d+)?:[\w.\-]+").unwrap();
        for cap in mention_regex.captures_iter(text_to_process) {
            if let Some(discord_id) = cap.get(1) {
                let mention = format!("<@{}>", discord_id.as_str());
                processed_body = processed_body.replace(&cap[0], &mention);
            }
        }

        {
            let emotes = self.cache.d_emotes.read();
            let emote_regex = regex::Regex::new(r":(\w+):").unwrap();
            for cap in emote_regex.captures_iter(body) {
                if let Some(name) = cap.get(1)
                    && let Some(discord_emote) = emotes.get(name.as_str())
                {
                    processed_body = processed_body.replace(&cap[0], discord_emote);
                }
            }
        }

        if processed_body.len() > DISCORD_MESSAGE_LIMIT {
            processed_body.truncate(DISCORD_MESSAGE_LIMIT);
            processed_body.push('…');
        }

        // Get channel ID
        let channel_id = self
            .db
            .get_channel(room_id)
            .await?
            .ok_or_else(|| BridgeError::NotFound)?;

        // Get webhook
        let webhook = self.get_or_create_webhook(&channel_id).await?;

        // Edit the message
        self.edit_webhook_message(&webhook, &discord_msg_id, &processed_body)
            .await?;

        tracing::info!(
            "Edited Discord message {} from Matrix event {}",
            discord_msg_id,
            new_event_id
        );

        Ok(())
    }

    async fn get_room_members(
        &self,
        room_id: &str,
    ) -> crate::error::Result<HashMap<String, crate::cache::MatrixUser>> {
        // Check cache first
        {
            let members = self.cache.m_members.read();
            if let Some(cached) = members.get(room_id) {
                return Ok(cached.clone());
            }
        }

        // Fetch from homeserver
        let resp = self
            .matrix
            .send_request(
                hyper::Method::GET,
                &format!("/rooms/{}/joined_members", urlencoding::encode(room_id)),
                None,
                None,
            )
            .await?;

        let mut members = HashMap::new();
        if let Some(joined) = resp["joined"].as_object() {
            for (user_id, user_data) in joined {
                members.insert(
                    user_id.clone(),
                    crate::cache::MatrixUser {
                        avatar_url: user_data["avatar_url"].as_str().map(String::from),
                        display_name: user_data["display_name"].as_str().map(String::from),
                    },
                );
            }
        }

        // Cache the result
        {
            let mut cache = self.cache.m_members.write();
            cache.insert(room_id.to_string(), members.clone());
        }

        Ok(members)
    }

    async fn handle_redaction_event(&self, event: &Value) -> crate::error::Result<()> {
        let room_id = event["room_id"].as_str().unwrap();
        let redacts = event["redacts"].as_str().unwrap();

        // Look up the Discord message ID from cache
        let discord_msg_id = {
            let messages = self.cache.m_messages.read();
            messages.get(redacts).cloned()
        };

        let discord_msg_id = if let Some(id) = discord_msg_id {
            id
        } else {
            tracing::debug!("No Discord message found for redaction of {}", redacts);
            return Ok(());
        };

        // Get the channel ID
        let channel_id = if let Some(id) = self.db.get_channel(room_id).await? {
            id
        } else {
            tracing::warn!("Room {} not bridged, cannot redact message", room_id);
            return Ok(());
        };

        // Get webhook
        let webhook = match self.get_or_create_webhook(&channel_id).await {
            Ok(wh) => wh,
            Err(e) => {
                tracing::error!("Failed to get webhook for channel {}: {}", channel_id, e);
                return Ok(());
            }
        };

        // Delete the Discord message
        match self.delete_webhook_message(&webhook, &discord_msg_id).await {
            Ok(()) => {
                tracing::info!(
                    "Deleted Discord message {} due to Matrix redaction",
                    discord_msg_id
                );

                // Clean up cache
                self.cache.m_messages.write().remove(redacts);
                self.cache.d_messages.write().remove(&discord_msg_id);
            }
            Err(e) => {
                tracing::error!("Failed to delete Discord message {}: {}", discord_msg_id, e);
            }
        }

        Ok(())
    }

    async fn handle_reaction_event(&self, event: &Value) -> crate::error::Result<()> {
        let room_id = event["room_id"].as_str().unwrap();
        let sender = event["sender"].as_str().unwrap();
        let event_id = event["event_id"].as_str().unwrap();
        let content = &event["content"];

        // Ignore reactions from Discord puppet users
        if sender.starts_with("@_discord_") {
            return Ok(());
        }

        // Get the related event
        let relates_to = content
            .get("m.relates_to")
            .ok_or_else(|| BridgeError::Matrix("Reaction missing m.relates_to".to_string()))?;

        let target_event_id = relates_to["event_id"]
            .as_str()
            .ok_or_else(|| BridgeError::Matrix("Reaction missing event_id".to_string()))?;

        let reaction_key = relates_to["key"]
            .as_str()
            .ok_or_else(|| BridgeError::Matrix("Reaction missing key".to_string()))?;

        // Find the Discord message ID
        let discord_msg_id = {
            let messages = self.cache.m_messages.read();
            messages.get(target_event_id).cloned()
        };

        let discord_msg_id = if let Some(id) = discord_msg_id {
            id
        } else {
            tracing::debug!("No Discord message found for reaction");
            return Ok(());
        };

        // Get channel ID
        let channel_id = self
            .db
            .get_channel(room_id)
            .await?
            .ok_or_else(|| BridgeError::NotFound)?;

        // Convert Matrix reaction to Discord emoji
        // For custom emojis in :name: format, look them up in cache
        let discord_emoji = if reaction_key.starts_with(':') && reaction_key.ends_with(':') {
            let name = reaction_key.trim_matches(':');
            let emotes = self.cache.d_emotes.read();

            // Extract just the emoji ID from the Discord format
            if let Some(discord_format) = emotes.get(name) {
                // Discord format is <:name:id> or <a:name:id>
                let id_regex = regex::Regex::new(r":(\d+)>$").unwrap();
                if let Some(cap) = id_regex.captures(discord_format) {
                    format!("{}:{}", name, cap.get(1).unwrap().as_str())
                } else {
                    // Fallback to Unicode
                    urlencoding::encode(reaction_key).to_string()
                }
            } else {
                // Not found, might be a Unicode emoji represented as :name:
                urlencoding::encode(reaction_key).to_string()
            }
        } else {
            // Unicode emoji
            urlencoding::encode(reaction_key).to_string()
        };

        tracing::info!(
            "Adding reaction {} to Discord message {}",
            discord_emoji,
            discord_msg_id
        );

        // Add reaction via Discord API
        let url = format!(
            "https://discord.com/api/v10/channels/{channel_id}/messages/{discord_msg_id}/reactions/{discord_emoji}/@me"
        );

        let response = self
            .discord_http
            .put(&url)
            .header(
                "Authorization",
                format!("Bot {}", self.config.discord_token),
            )
            .send()
            .await
            .map_err(|e| {
                BridgeError::Discord(serenity::Error::from(std::io::Error::other(e.to_string())))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            tracing::error!("Failed to add Discord reaction {}: {}", status, error_text);
            return Err(BridgeError::Discord(serenity::Error::from(
                std::io::Error::other(format!("Discord reaction failed: {status}")),
            )));
        }

        // Cache the reaction mapping for removal
        let cache_key = format!("{discord_msg_id}:{sender}:{reaction_key}");
        self.cache
            .m_messages
            .write()
            .insert(cache_key, event_id.to_string());

        Ok(())
    }

    async fn handle_typing_event(&self, event: &Value) -> crate::error::Result<()> {
        let room_id = event["room_id"].as_str().unwrap_or("");
        let content = &event["content"];

        let user_ids = content["user_ids"]
            .as_array()
            .ok_or_else(|| BridgeError::Matrix("Typing event missing user_ids".to_string()))?;

        // Check if room is bridged
        let channel_id = match self.db.get_channel(room_id).await? {
            Some(id) => id,
            None => return Ok(()), // Not bridged, ignore
        };

        // Process typing indicators
        for user_id in user_ids {
            if let Some(uid) = user_id.as_str() {
                // Ignore Discord puppet users
                if uid.starts_with("@_discord_") || uid == self.config.full_user_id() {
                    continue;
                }

                tracing::debug!("User {} is typing in room {}", uid, room_id);

                // Note: Discord doesn't have a direct typing indicator API that bots can trigger
                // via webhooks. The typing indicator is only available for bot users posting
                // directly, not through webhooks. We could trigger it but it would appear
                // as the bot, not as the bridged user. For now, we'll just log it.
                // In a full implementation, you might want to:
                // 1. Use the bot to send typing via POST /channels/{channel_id}/typing
                // 2. But this would show as the bot, not the user
                // This is a limitation of Discord's API with webhooks.

                let url = format!("https://discord.com/api/v10/channels/{channel_id}/typing");

                if let Err(e) = self
                    .discord_http
                    .post(&url)
                    .header(
                        "Authorization",
                        format!("Bot {}", self.config.discord_token),
                    )
                    .send()
                    .await
                {
                    tracing::debug!("Failed to send typing indicator: {}", e);
                }
            }
        }

        Ok(())
    }

    async fn get_or_create_webhook(&self, channel_id: &str) -> crate::error::Result<WebhookData> {
        // Check cache first
        {
            let webhooks = self.cache.d_webhooks.read();
            if let Some(info) = webhooks.get(channel_id) {
                return Ok(WebhookData {
                    id: info.id.clone(),
                    token: info.token.clone(),
                });
            }
        }

        // Fetch existing webhooks
        let url = format!("https://discord.com/api/v10/channels/{channel_id}/webhooks");

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
                BridgeError::Discord(serenity::Error::from(std::io::Error::other(e.to_string())))
            })?;

        if !response.status().is_success() {
            return Err(BridgeError::Discord(serenity::Error::from(
                std::io::Error::other(format!("Failed to fetch webhooks: {}", response.status())),
            )));
        }

        let webhooks: Vec<Value> = response
            .json()
            .await
            .map_err(|e| BridgeError::Matrix(format!("Failed to parse webhooks response: {e}")))?;

        // Look for existing bridge webhook
        let existing = webhooks
            .iter()
            .find(|w| w["name"].as_str() == Some("matrix_bridge"));

        let webhook_data = if let Some(wh) = existing {
            WebhookData {
                id: wh["id"].as_str().unwrap().to_string(),
                token: wh["token"].as_str().unwrap().to_string(),
            }
        } else {
            // Create new webhook
            let create_url = format!("https://discord.com/api/v10/channels/{channel_id}/webhooks");
            let create_body = json!({
                "name": "matrix_bridge"
            });

            let response = self
                .discord_http
                .post(&create_url)
                .header(
                    "Authorization",
                    format!("Bot {}", self.config.discord_token),
                )
                .header("Content-Type", "application/json")
                .json(&create_body)
                .send()
                .await
                .map_err(|e| BridgeError::Matrix(format!("Failed to create webhook: {e}")))?;

            if !response.status().is_success() {
                let error_text = response.text().await.unwrap_or_default();
                return Err(BridgeError::Discord(serenity::Error::from(
                    std::io::Error::other(format!("Failed to create webhook: {error_text}")),
                )));
            }

            let webhook: Value = response.json().await.map_err(|e| {
                BridgeError::Matrix(format!("Failed to parse webhook response: {e}"))
            })?;

            WebhookData {
                id: webhook["id"].as_str().unwrap().to_string(),
                token: webhook["token"].as_str().unwrap().to_string(),
            }
        };

        // Cache it
        {
            let mut webhooks = self.cache.d_webhooks.write();
            webhooks.insert(
                channel_id.to_string(),
                crate::cache::WebhookInfo {
                    id: webhook_data.id.clone(),
                    token: webhook_data.token.clone(),
                },
            );
        }

        Ok(webhook_data)
    }

    async fn send_webhook_message(
        &self,
        webhook: &WebhookData,
        content: &str,
        username: &str,
        avatar_url: Option<&str>,
        thread_id: Option<&str>,
    ) -> crate::error::Result<String> {
        let mut url = format!(
            "https://discord.com/api/v10/webhooks/{}/{}?wait=true",
            webhook.id, webhook.token
        );

        if let Some(tid) = thread_id {
            url.push_str("&thread_id=");
            url.push_str(tid);
        }

        let mut body = json!({
            "content": content,
            "username": username,
            "allowed_mentions": {
                "parse": ["users", "roles"]
            }
        });

        if let Some(avatar) = avatar_url {
            body["avatar_url"] = json!(avatar);
        }

        let response = self
            .discord_http
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| BridgeError::Matrix(format!("Failed to send webhook message: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(BridgeError::Discord(serenity::Error::from(
                std::io::Error::other(format!("Webhook send failed {status}: {error_text}")),
            )));
        }

        let message: Value = response
            .json()
            .await
            .map_err(|e| BridgeError::Matrix(format!("Failed to parse webhook response: {e}")))?;

        Ok(message["id"].as_str().unwrap().to_string())
    }

    async fn send_webhook_with_file(
        &self,
        webhook: &WebhookData,
        content: &str,
        username: &str,
        avatar_url: Option<&str>,
        filename: &str,
        file_data: &[u8],
    ) -> crate::error::Result<String> {
        let url = format!(
            "https://discord.com/api/v10/webhooks/{}/{}?wait=true",
            webhook.id, webhook.token
        );

        // Build multipart form
        let form = reqwest::multipart::Form::new()
            .text("username", username.to_string())
            .text("content", content.to_string())
            .part(
                "file",
                reqwest::multipart::Part::bytes(file_data.to_vec()).file_name(filename.to_string()),
            );

        let mut request = self.discord_http.post(&url).multipart(form);

        if let Some(avatar) = avatar_url {
            request = request.header("X-Avatar-URL", avatar);
        }

        let response = request
            .send()
            .await
            .map_err(|e| BridgeError::Matrix(format!("Failed to send webhook with file: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(BridgeError::Discord(serenity::Error::from(
                std::io::Error::other(format!("Webhook file send failed {status}: {error_text}")),
            )));
        }

        let message: Value = response
            .json()
            .await
            .map_err(|e| BridgeError::Matrix(format!("Failed to parse webhook response: {e}")))?;

        Ok(message["id"].as_str().unwrap().to_string())
    }

    async fn edit_webhook_message(
        &self,
        webhook: &WebhookData,
        message_id: &str,
        content: &str,
    ) -> crate::error::Result<()> {
        let url = format!(
            "https://discord.com/api/v10/webhooks/{}/{}/messages/{}",
            webhook.id, webhook.token, message_id
        );

        let body = json!({
            "content": content
        });

        let response = self
            .discord_http
            .patch(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| BridgeError::Matrix(format!("Failed to edit webhook message: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();

            // 404 means message was already deleted, which is fine
            if status == 404 {
                tracing::debug!("Message {} already deleted", message_id);
                return Ok(());
            }

            return Err(BridgeError::Matrix(format!(
                "Webhook edit failed {status}: {error_text}"
            )));
        }

        Ok(())
    }

    async fn delete_webhook_message(
        &self,
        webhook: &WebhookData,
        message_id: &str,
    ) -> crate::error::Result<()> {
        let url = format!(
            "https://discord.com/api/v10/webhooks/{}/{}/messages/{}",
            webhook.id, webhook.token, message_id
        );

        let response = self.discord_http.delete(&url).send().await.map_err(|e| {
            BridgeError::Discord(serenity::Error::from(std::io::Error::other(e.to_string())))
        })?;

        if !response.status().is_success() {
            let status = response.status();

            // 404 means message was already deleted, which is fine
            if status == 404 {
                tracing::debug!("Message {} already deleted", message_id);
                return Ok(());
            }

            let error_text = response.text().await.unwrap_or_default();
            return Err(BridgeError::Discord(serenity::Error::from(
                std::io::Error::other(format!("Webhook delete failed {status}: {error_text}")),
            )));
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
struct WebhookData {
    id: String,
    token: String,
}
