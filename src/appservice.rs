use crate::{
    cache::Cache, config::Config, db::Database, error::BridgeError, matrix_client::MatrixClient,
};
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use ruma::events::room::message::RoomMessageEventContent;
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
}

impl AppService {
    pub fn new(config: Config, matrix: Arc<MatrixClient>, db: Database, cache: Cache) -> Self {
        let discord_http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            config,
            matrix,
            db,
            cache,
            discord_http,
        }
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let addr = format!("127.0.0.1:{}", self.config.port);
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

        if method == Method::PUT && path.starts_with("/transactions/") {
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
            "m.room.redaction" => self.handle_redaction_event(event).await,
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
        if state_key == self.config.full_user_id() {
            if let Some(true) = content["is_direct"].as_bool() {
                tracing::info!("Ignoring invite from user")
                // tracing::info!("Joining DM room {}", room_id);
                // self.matrix.join_room(room_id, None).await?;
            }
        }

        Ok(())
    }

    async fn handle_message_event(&self, event: &Value) -> crate::error::Result<()> {
        let room_id = event["room_id"].as_str().unwrap();
        let sender = event["sender"].as_str().unwrap();
        let event_id = event["event_id"].as_str().unwrap();
        let content = &event["content"];
        let body = content["body"].as_str().unwrap_or("");

        // Ignore our own messages (bot and puppet users)
        if sender.starts_with("@_discord_") || sender == self.config.full_user_id() {
            return Ok(());
        }

        // Handle bridge commands
        if body.starts_with("!bridge ") {
            return self.handle_bridge_command(room_id, sender, body).await;
        }

        // Check if this is an edit
        let relates_to = content.get("m.relates_to");
        if let Some(rel) = relates_to {
            if rel["rel_type"].as_str() == Some("m.replace") {
                let original_event_id = rel["event_id"].as_str().unwrap_or("");
                return self
                    .handle_message_edit(room_id, sender, event_id, original_event_id, content)
                    .await;
            }
        }

        // Check if room is bridged
        if let Some(channel_id) = self.db.get_channel(room_id).await? {
            return self
                .forward_to_discord(room_id, sender, event_id, content, &channel_id)
                .await;
        }

        Ok(())
    }

    async fn handle_bridge_command(
        &self,
        room_id: &str,
        _sender: &str,
        body: &str,
    ) -> crate::error::Result<()> {
        let parts: Vec<&str> = body.split_whitespace().collect();
        if parts.len() < 2 {
            let content = RoomMessageEventContent::text_plain("Usage: !bridge <channel_id>");
            let _ = self.matrix.send_message(room_id, content, None).await;
            return Ok(());
        }

        let channel_id = parts[1];

        // Validate channel ID format (Discord snowflakes are numeric)
        if !channel_id.chars().all(|c| c.is_ascii_digit()) {
            let content = RoomMessageEventContent::text_plain(
                "Invalid channel ID format. Channel IDs should be numeric.",
            );
            let _ = self.matrix.send_message(room_id, content, None).await;
            return Ok(());
        }

        tracing::info!(
            "Bridge command: linking room {} to Discord channel {}",
            room_id,
            channel_id
        );

        // Verify we have access to this channel
        match self.verify_discord_channel(channel_id).await {
            Ok(channel_info) => {
                // Check if channel is already bridged
                let existing_channels = self.db.list_channels().await?;
                if existing_channels.contains(&channel_id.to_string()) {
                    let content = RoomMessageEventContent::text_plain(
                        "This Discord channel is already bridged to another room.",
                    );
                    let _ = self.matrix.send_message(room_id, content, None).await;
                    return Ok(());
                }

                // Create the bridge link
                self.db.add_room(room_id, channel_id).await?;

                // Update cache
                let room_alias = self.matrix.matrixify_room(channel_id);
                self.cache
                    .m_rooms
                    .write()
                    .insert(room_alias, room_id.to_string());

                // Send confirmation message
                let content = RoomMessageEventContent::text_plain(&format!(
                    "✓ Room successfully bridged to Discord channel #{} ({})",
                    channel_info.name, channel_id
                ));
                let _ = self.matrix.send_message(room_id, content, None).await;

                tracing::info!(
                    "Successfully bridged room {} to channel {}",
                    room_id,
                    channel_id
                );
            }
            Err(e) => {
                tracing::error!("Failed to verify Discord channel {}: {}", channel_id, e);
                let content = RoomMessageEventContent::text_plain(&format!(
                    "Failed to bridge: {}. Make sure the channel exists and the bot has access.",
                    e
                ));
                let _ = self.matrix.send_message(room_id, content, None).await;
            }
        }

        Ok(())
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
                BridgeError::Discord(serenity::Error::from(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e.to_string(),
                )))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(BridgeError::Discord(serenity::Error::from(
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Discord API error {}: {}", status, error_text),
                ),
            )));
        }

        let channel_data: Value = response
            .json()
            .await
            .map_err(|e| BridgeError::Matrix(format!("Failed to parse Discord response: {}", e)))?;

        // Check if it's a text channel (type 0) or news channel (type 5)
        let channel_type = channel_data["type"].as_u64().unwrap_or(999);
        if channel_type != 0 && channel_type != 5 {
            return Err(BridgeError::Config(
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
        })
    }

    async fn forward_to_discord(
        &self,
        room_id: &str,
        sender: &str,
        event_id: &str,
        content: &Value,
        channel_id: &str,
    ) -> crate::error::Result<()> {
        let body = content["body"].as_str().unwrap_or("");
        let formatted_body = content["formatted_body"].as_str();

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

        // Handle Matrix emotes -> Discord emotes
        {
            let emotes = self.cache.d_emotes.read();
            let emote_regex = regex::Regex::new(r":(\w+):").unwrap();
            for cap in emote_regex.captures_iter(body) {
                if let Some(name) = cap.get(1) {
                    if let Some(discord_emote) = emotes.get(name.as_str()) {
                        processed_body = processed_body.replace(&cap[0], discord_emote);
                    }
                }
            }
        }

        // Truncate to Discord's message limit
        const DISCORD_MESSAGE_LIMIT: usize = 2000;
        if processed_body.len() > DISCORD_MESSAGE_LIMIT {
            processed_body.truncate(DISCORD_MESSAGE_LIMIT);
            processed_body.push_str("…");
        }

        // Get member info for avatar and display name
        let members = self.get_room_members(room_id).await?;
        let member = members.get(sender);

        let display_name = member
            .and_then(|m| m.display_name.as_ref())
            .map(|s| s.as_str())
            .unwrap_or_else(|| sender.split(':').next().unwrap_or(sender));

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

        let discord_msg_id = match discord_msg_id {
            Some(id) => id,
            None => {
                tracing::debug!("No Discord message found for edit of {}", original_event_id);
                return Ok(());
            }
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
                if let Some(name) = cap.get(1) {
                    if let Some(discord_emote) = emotes.get(name.as_str()) {
                        processed_body = processed_body.replace(&cap[0], discord_emote);
                    }
                }
            }
        }

        const DISCORD_MESSAGE_LIMIT: usize = 2000;
        if processed_body.len() > DISCORD_MESSAGE_LIMIT {
            processed_body.truncate(DISCORD_MESSAGE_LIMIT);
            processed_body.push_str("…");
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

        let discord_msg_id = match discord_msg_id {
            Some(id) => id,
            None => {
                tracing::debug!("No Discord message found for redaction of {}", redacts);
                return Ok(());
            }
        };

        // Get the channel ID
        let channel_id = match self.db.get_channel(room_id).await? {
            Some(id) => id,
            None => {
                tracing::warn!("Room {} not bridged, cannot redact message", room_id);
                return Ok(());
            }
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
            Ok(_) => {
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
        let url = format!(
            "https://discord.com/api/v10/channels/{}/webhooks",
            channel_id
        );

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
                BridgeError::Discord(serenity::Error::from(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    e.to_string(),
                )))
            })?;

        if !response.status().is_success() {
            return Err(BridgeError::Discord(serenity::Error::from(
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Failed to fetch webhooks: {}", response.status()),
                ),
            )));
        }

        let webhooks: Vec<Value> = response.json().await.map_err(|e| {
            BridgeError::Matrix(format!("Failed to parse webhooks response: {}", e))
        })?;

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
            let create_url = format!(
                "https://discord.com/api/v10/channels/{}/webhooks",
                channel_id
            );
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
                .map_err(|e| BridgeError::Matrix(format!("Failed to create webhook: {}", e)))?;

            if !response.status().is_success() {
                let error_text = response.text().await.unwrap_or_default();
                return Err(BridgeError::Discord(serenity::Error::from(
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("Failed to create webhook: {}", error_text),
                    ),
                )));
            }

            let webhook: Value = response.json().await.map_err(|e| {
                BridgeError::Matrix(format!("Failed to parse webhook response: {}", e))
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
    ) -> crate::error::Result<String> {
        let url = format!(
            "https://discord.com/api/v10/webhooks/{}/{}?wait=true",
            webhook.id, webhook.token
        );

        let mut body = json!({
            "content": content,
            "username": username,
            "allowed_mentions": {
                "parse": ["users"]
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
            .map_err(|e| BridgeError::Matrix(format!("Failed to send webhook message: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(BridgeError::Discord(serenity::Error::from(
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Webhook send failed {}: {}", status, error_text),
                ),
            )));
        }

        let message: Value = response
            .json()
            .await
            .map_err(|e| BridgeError::Matrix(format!("Failed to parse webhook response: {}", e)))?;

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
            .map_err(|e| BridgeError::Matrix(format!("Failed to edit webhook message: {}", e)))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();

            // 404 means message was already deleted, which is fine
            if status == 404 {
                tracing::debug!("Message {} already deleted", message_id);
                return Ok(());
            }

            return Err(BridgeError::Matrix(format!(
                "Webhook edit failed {}: {}",
                status, error_text
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
            BridgeError::Discord(serenity::Error::from(std::io::Error::new(
                std::io::ErrorKind::Other,
                e.to_string(),
            )))
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
                std::io::Error::new(
                    std::io::ErrorKind::Other,
                    format!("Webhook delete failed {}: {}", status, error_text),
                ),
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

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct ChannelInfo {
    id: String,
    name: String,
    guild_id: Option<String>,
}
