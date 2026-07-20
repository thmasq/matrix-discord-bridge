use crate::{
    admin_commands::AdminCommandHandler, cache::Cache, config::Config, db::Database,
    error::BridgeError, matrix_client::MatrixClient, user_commands::UserCommandHandler,
};
use hmac::KeyInit;
use hmac::{Hmac, Mac};
use http_body_util::{BodyExt, Full};
use hyper::{
    Method, Request, Response, StatusCode,
    body::{Bytes, Incoming},
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::{
    net::TcpListener,
    sync::{Mutex, mpsc},
};

type HmacSha256 = Hmac<Sha256>;

const DISCORD_MESSAGE_LIMIT: usize = 2000;
const MAX_BODY_SIZE: usize = 20 * 1024 * 1024; // 20 MB

static MENTION_REGEX: OnceLock<regex::Regex> = OnceLock::new();
static EMOTE_REGEX: OnceLock<regex::Regex> = OnceLock::new();
static ID_REGEX: OnceLock<regex::Regex> = OnceLock::new();
static HTML_REPLY_REGEX: OnceLock<regex::Regex> = OnceLock::new();

pub struct AppService {
    config: Config,
    matrix: Arc<MatrixClient>,
    db: Database,
    cache: Cache,
    discord_http: reqwest::Client,
    admin_handler: AdminCommandHandler,
    user_handler: UserCommandHandler,
    event_sender: mpsc::Sender<Value>,
    event_receiver: Mutex<Option<mpsc::Receiver<Value>>>,
}

#[derive(Debug, Clone)]
struct WebhookData {
    id: String,
    token: String,
}

pub fn ts_to_snowflake(ts_ms: u64) -> u64 {
    const DISCORD_EPOCH: u64 = 1420070400000;
    if ts_ms < DISCORD_EPOCH {
        return 0;
    }
    (ts_ms - DISCORD_EPOCH) << 22
}

pub fn snowflake_to_ts(snowflake: &str) -> u64 {
    let id = snowflake.parse::<u64>().unwrap_or(0);
    if id == 0 {
        return 0;
    }
    (id >> 22) + 1420070400000
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

        let user_handler = UserCommandHandler::new(matrix.clone(), db.clone(), cache.clone());

        let (tx, rx) = mpsc::channel(10000);

        Self {
            config,
            matrix,
            db,
            cache,
            discord_http,
            admin_handler,
            user_handler,
            event_sender: tx,
            event_receiver: Mutex::new(Some(rx)),
        }
    }

    pub async fn run(self: Arc<Self>) -> anyhow::Result<()> {
        let rx = self.event_receiver.lock().await.take();
        if let Some(mut rx) = rx {
            let service = self.clone();
            tokio::spawn(async move {
                tracing::info!("Started background event processor task");
                while let Some(event) = rx.recv().await {
                    if let Err(e) = service.handle_event(&event).await {
                        tracing::error!("Error handling event: {}", e);
                    }
                }
            });
        }

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
        } else if method == Method::GET && path.starts_with("/avatar/") {
            self.handle_avatar_request(req).await
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
        let token_query = query
            .split('&')
            .find(|s| s.starts_with("access_token="))
            .and_then(|s| s.strip_prefix("access_token="));

        let token_header = req
            .headers()
            .get("Authorization")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "));

        let query_valid = token_query.is_some_and(|t| {
            constant_time_eq::constant_time_eq(t.as_bytes(), self.config.hs_token.as_bytes())
        });

        let header_valid = token_header.is_some_and(|t| {
            constant_time_eq::constant_time_eq(t.as_bytes(), self.config.hs_token.as_bytes())
        });

        if !query_valid && !header_valid {
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
        let limited_body = http_body_util::Limited::new(req.into_body(), MAX_BODY_SIZE);

        let body = limited_body
            .collect()
            .await
            .map_err(|e| {
                crate::error::BridgeError::Matrix(format!("Body too large or read error: {e}"))
            })?
            .to_bytes();

        let json: Value = serde_json::from_slice(&body)?;

        let Some(events) = json.get("events").and_then(|e| e.as_array()) else {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from(
                    serde_json::to_vec(&serde_json::json!({
                        "errcode": "M_BAD_JSON",
                        "error": "Missing or invalid 'events' array"
                    }))
                    .unwrap(),
                )))
                .unwrap());
        };

        // Process events
        for event in events {
            if let Err(e) = self.event_sender.send(event.clone()).await {
                tracing::error!("Failed to enqueue event: {}", e);
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
        let sender = event["sender"].as_str().unwrap();
        let state_key = event["state_key"].as_str().unwrap();
        let content = &event["content"];
        let membership = content["membership"].as_str().unwrap_or("");

        // Clear member cache for this room
        self.cache.m_members.remove(room_id);

        // If it's an event targeting our bot user
        if state_key == self.config.full_user_id() {
            if membership == "invite" {
                // Extract room name from invite_room_state if available
                let mut room_name = None;
                if let Some(unsigned) = event.get("unsigned")
                    && let Some(invite_room_state) =
                        unsigned.get("invite_room_state").and_then(|s| s.as_array())
                {
                    for state in invite_room_state {
                        if state.get("type").and_then(|t| t.as_str()) == Some("m.room.name") {
                            room_name = state
                                .get("content")
                                .and_then(|c| c.get("name"))
                                .and_then(|n| n.as_str());
                        }
                    }
                }

                self.db.add_invite(room_id, sender, room_name).await?;

                // Notify admin config room
                if let Some(config_room) = &self.config.config_room_id {
                    // Prevent ping loops if the bot is invited to its own config room
                    if config_room != room_id {
                        let msg = format!(
                            "Received new invite to `{room_id}` from `{sender}`. Use `!invite list` to manage."
                        );
                        let msg_content =
                            ruma::events::room::message::RoomMessageEventContent::text_plain(msg);
                        let _ = self
                            .matrix
                            .send_message(config_room, msg_content, None)
                            .await;
                    }
                }
            } else if membership == "leave" || membership == "ban" {
                // If the bot leaves, is kicked, or an invite is retracted, clean up the DB
                let _ = self.db.remove_invite_by_room(room_id).await;
            }
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
        let body = content["body"].as_str().unwrap_or("");

        if msgtype == "m.text" && body.starts_with('!') {
            if let Some(ref config_room) = self.config.config_room_id {
                if room_id == config_room {
                    return self
                        .admin_handler
                        .handle_command(room_id, sender, body)
                        .await;
                }
            }
        }

        if !self
            .user_handler
            .process_event(room_id, sender, body)
            .await?
        {
            return Ok(());
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
        if let Some(bridge) = self.db.get_bridge(room_id).await?
            && bridge.m2d_enabled
        {
            return self
                .forward_message_to_discord(
                    room_id,
                    sender,
                    event_id,
                    content,
                    &bridge.channel_id,
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
        let Some(bridge) = self.db.get_bridge(room_id).await? else {
            return Ok(());
        };
        if !bridge.m2d_enabled {
            return Ok(());
        }
        let channel_id = bridge.channel_id;

        let url = content["url"]
            .as_str()
            .ok_or_else(|| BridgeError::Matrix("Sticker missing url field".to_string()))?;

        let body = content["body"].as_str().unwrap_or("sticker");

        let file_body = self.download_mxc_stream(url).await?;

        let members = self.get_room_members(room_id).await?;

        let reply_embed = self
            .build_reply_embed(room_id, content, &members, &channel_id)
            .await;

        let (display_name, avatar_url) = self.resolve_sender_profile(sender, &members);

        tracing::info!(
            "Forwarding sticker from {} to Discord channel {}",
            sender,
            channel_id
        );

        // Get webhook
        let webhook = self.get_or_create_webhook(&channel_id).await?;

        // Determine filename from content or use default
        let info = content.get("info");
        let filename =
            info.and_then(|i| i["mimetype"].as_str())
                .map_or("sticker.png", |mimetype| {
                    if mimetype.contains("gif") {
                        "sticker.gif"
                    } else if mimetype.contains("webp") {
                        "sticker.webp"
                    } else {
                        "sticker.png"
                    }
                });

        // Send sticker stream as an image file to Discord
        let discord_msg_id = self
            .send_webhook_with_stream(
                &webhook,
                body,
                display_name,
                avatar_url.as_deref(),
                filename,
                file_body,
                reply_embed,
            )
            .await?;

        // Cache the mapping
        self.cache
            .insert_message_mapping(event_id.to_string(), discord_msg_id);

        tracing::info!("Successfully sent Matrix sticker {} to Discord", event_id);

        Ok(())
    }

    async fn handle_emoji_update(&self, event: &Value) -> crate::error::Result<()> {
        let room_id = event["room_id"].as_str().unwrap();

        tracing::info!("Emoji pack updated in room {}, refreshing cache", room_id);

        // Clear the cache for this room
        self.cache.m_custom_emojis.remove(room_id);

        // Fetch fresh emoji data
        match self.matrix.fetch_room_emojis(room_id, None).await {
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

        let mut actual_body = body.to_string();
        let mut text_to_process = formatted_body.map(std::string::ToString::to_string);

        let members = self.get_room_members(room_id).await?;

        let reply_embed = self
            .build_reply_embed(room_id, content, &members, channel_id)
            .await;

        if let Some(relates_to) = content.get("m.relates_to")
            && relates_to.get("m.in_reply_to").is_some()
        {
            actual_body = Self::strip_matrix_reply_fallback(&actual_body);
            text_to_process = text_to_process.map(|s| Self::strip_matrix_reply_fallback_html(&s));
        }

        let processed_body =
            self.process_matrix_text_for_discord(&actual_body, text_to_process.as_deref());

        let (display_name, avatar_url) = self.resolve_sender_profile(sender, &members);

        tracing::info!(
            "Forwarding message from {} to Discord channel {}",
            sender,
            channel_id
        );

        let webhook = self.get_or_create_webhook(channel_id).await?;

        let discord_msg_id = self
            .send_webhook_message(
                &webhook,
                &processed_body,
                display_name,
                avatar_url.as_deref(),
                None,
                reply_embed,
            )
            .await?;

        self.cache
            .insert_message_mapping(event_id.to_string(), discord_msg_id);

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

        let original_body = content["body"].as_str().unwrap_or("attachment");

        let is_msc4193_spoiler = content["page.codeberg.everypizza.msc4193.spoiler"]
            .as_bool()
            .unwrap_or(false);

        let is_spoiler = original_body.starts_with("SPOILER_")
            || original_body.starts_with("spoiler_")
            || is_msc4193_spoiler;

        let mut filename = original_body.to_string();
        if is_spoiler && !filename.starts_with("SPOILER_") {
            filename = format!("SPOILER_{}", original_body.trim_start_matches("spoiler_"));
        }

        let message_content = "";

        let file_body = self.download_mxc_stream(url).await?;

        let members = self.get_room_members(room_id).await?;

        let reply_embed = self
            .build_reply_embed(room_id, content, &members, channel_id)
            .await;

        let (display_name, avatar_url) = self.resolve_sender_profile(sender, &members);

        tracing::info!(
            "Forwarding {} (Spoiler: {}) from {} to Discord channel {}",
            msgtype,
            is_spoiler,
            sender,
            channel_id
        );

        let webhook = self.get_or_create_webhook(channel_id).await?;

        // Upload to Discord and send stream
        let discord_msg_id = self
            .send_webhook_with_stream(
                &webhook,
                message_content,
                display_name,
                avatar_url.as_deref(),
                &filename,
                file_body,
                reply_embed,
            )
            .await?;

        // Cache the mapping
        self.cache
            .insert_message_mapping(event_id.to_string(), discord_msg_id);

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
        let discord_msg_id = self.cache.m_messages.get(original_event_id);

        let Some(discord_msg_id) = discord_msg_id else {
            tracing::debug!("No Discord message found for edit of {}", original_event_id);
            return Ok(());
        };

        // Get the new content
        let new_content = content
            .get("m.new_content")
            .ok_or_else(|| BridgeError::Matrix("Edit missing m.new_content".to_string()))?;

        let body = new_content["body"].as_str().unwrap_or("");
        let formatted_body = new_content["formatted_body"].as_str();

        let processed_body = self.process_matrix_text_for_discord(body, formatted_body);

        let bridge = self
            .db
            .get_bridge(room_id)
            .await?
            .ok_or_else(|| BridgeError::NotFound)?;

        if !bridge.m2d_enabled {
            return Ok(());
        }
        let channel_id = bridge.channel_id;

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
        if let Some(cached) = self.cache.m_members.get(room_id) {
            return Ok(cached);
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
        self.cache
            .m_members
            .insert(room_id.to_string(), members.clone());

        Ok(members)
    }

    async fn handle_redaction_event(&self, event: &Value) -> crate::error::Result<()> {
        let room_id = event["room_id"].as_str().unwrap();
        let redacts = event["redacts"].as_str().unwrap();

        let mapped_val = self.cache.m_messages.get(redacts);

        let Some(mapped_val) = mapped_val else {
            tracing::debug!("No mapping found for redaction of {}", redacts);
            return Ok(());
        };

        // Get the bridge
        let Some(bridge) = self.db.get_bridge(room_id).await? else {
            tracing::warn!("Room {} not bridged, cannot redact message", room_id);
            return Ok(());
        };

        if !bridge.m2d_enabled {
            return Ok(());
        }

        if mapped_val.starts_with("reaction|") {
            let parts: Vec<&str> = mapped_val.splitn(5, '|').collect();
            if parts.len() == 5 {
                let discord_msg_id = parts[1];
                let target_event_id = parts[2];
                let discord_emoji = parts[3];
                let reaction_key = parts[4];

                let count_key = format!("reaction_count|{}|{}", target_event_id, reaction_key);
                let cached_count: Option<u32> = self
                    .cache
                    .m_messages
                    .get(&count_key)
                    .and_then(|v| v.parse().ok());

                if let Some(count) = cached_count {
                    if count > 1 {
                        tracing::debug!(
                            "Fast path: Decrementing cached reaction count (now {})",
                            count - 1
                        );
                        self.cache
                            .m_messages
                            .insert(count_key, (count - 1).to_string());
                        self.cache.m_messages.invalidate(redacts);
                        return Ok(());
                    }
                }

                tracing::debug!("Slow path: Querying Matrix API for remaining reactions");
                let mut active = 0;
                if let Ok(reactions) = self.matrix.get_reactions(room_id, target_event_id).await {
                    for r in reactions {
                        let r_event_id = r.get("event_id").and_then(|e| e.as_str()).unwrap_or("");

                        if r_event_id != redacts {
                            if let Some(content) = r.get("content") {
                                if let Some(rel) = content.get("m.relates_to") {
                                    if let Some(key) = rel.get("key").and_then(|k| k.as_str()) {
                                        if key == reaction_key {
                                            active += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                self.cache.m_messages.insert(count_key, active.to_string());

                if active == 0 {
                    tracing::info!(
                        "Removing reaction {} from Discord message {}",
                        discord_emoji,
                        discord_msg_id
                    );
                    let url = format!(
                        "https://discord.com/api/v10/channels/{}/messages/{}/reactions/{}/@me",
                        bridge.channel_id, discord_msg_id, discord_emoji
                    );
                    let _ = self
                        .discord_http
                        .delete(&url)
                        .header(
                            "Authorization",
                            format!("Bot {}", self.config.discord_token),
                        )
                        .send()
                        .await;
                }

                self.cache.m_messages.invalidate(redacts);
            }
            return Ok(());
        }

        let discord_msg_id = mapped_val;
        let sender = event["sender"].as_str().unwrap();
        let is_mod_deletion = match self.matrix.get_event(room_id, redacts).await {
            Ok(original_event) => original_event.sender != sender,
            Err(_) => {
                tracing::debug!(
                    "Failed to fetch event {}, assuming normal M->D deletion",
                    redacts
                );
                false
            }
        };

        if is_mod_deletion && !bridge.m2d_mod_deletions {
            tracing::debug!("Ignoring M->D mod deletion");
            return Ok(());
        }

        let channel_id = bridge.channel_id;

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
                self.cache
                    .remove_message_mapping(Some(redacts), Some(&discord_msg_id));
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
        let discord_msg_id = self.cache.m_messages.get(target_event_id);

        let Some(discord_msg_id) = discord_msg_id else {
            tracing::debug!("No Discord message found for reaction");
            return Ok(());
        };

        // Get channel ID
        let bridge = self
            .db
            .get_bridge(room_id)
            .await?
            .ok_or_else(|| BridgeError::NotFound)?;

        if !bridge.m2d_enabled {
            return Ok(());
        }
        let channel_id = bridge.channel_id;

        let mut resolved_discord_emoji = None;

        let id_regex = ID_REGEX.get_or_init(|| regex::Regex::new(r":(\d+)>$").unwrap());

        let shortcode_from_event = content.get("shortcode").and_then(|v| v.as_str());

        if reaction_key.starts_with("mxc://") {
            if let Some(sc) = shortcode_from_event {
                if let Some(discord_format) = self.cache.d_emotes.get(sc) {
                    if let Some(cap) = id_regex.captures(&discord_format) {
                        resolved_discord_emoji = Some(format!(
                            "{}%3A{}",
                            urlencoding::encode(sc),
                            cap.get(1).unwrap().as_str()
                        ));
                    }
                }
            }

            if resolved_discord_emoji.is_none() {
                for (name, mxc) in &self.cache.m_emotes {
                    if mxc == reaction_key {
                        if let Some(discord_format) = self.cache.d_emotes.get(&*name)
                            && let Some(cap) = id_regex.captures(&discord_format)
                        {
                            resolved_discord_emoji = Some(format!(
                                "{}%3A{}",
                                urlencoding::encode(&name),
                                cap.get(1).unwrap().as_str()
                            ));
                        }
                        break;
                    }
                }
            }

            if resolved_discord_emoji.is_none()
                && let Ok(room_emojis) = self.matrix.get_room_emojis(room_id, None).await
            {
                for (shortcode, mxc) in room_emojis {
                    if mxc == reaction_key {
                        if let Some(discord_format) = self.cache.d_emotes.get(&shortcode)
                            && let Some(cap) = id_regex.captures(&discord_format)
                        {
                            resolved_discord_emoji = Some(format!(
                                "{}%3A{}",
                                urlencoding::encode(&shortcode),
                                cap.get(1).unwrap().as_str()
                            ));
                        }
                        break;
                    }
                }
            }
        } else {
            let clean_name = reaction_key.trim_matches(':');

            if let Some(discord_format) = self.cache.d_emotes.get(clean_name) {
                resolved_discord_emoji = id_regex.captures(&discord_format).map(|cap| {
                    format!(
                        "{}%3A{}",
                        urlencoding::encode(clean_name),
                        cap.get(1).unwrap().as_str()
                    )
                });
            } else {
                resolved_discord_emoji = Some(urlencoding::encode(reaction_key).to_string());
            }
        }

        let Some(discord_emoji) = resolved_discord_emoji else {
            tracing::info!(
                "Ignoring unmappable Matrix reaction key (likely a Matrix-exclusive custom emoji): {}",
                reaction_key
            );
            return Ok(());
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

        tracing::info!("Adding reaction with {url}");

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
                BridgeError::Discord(Box::new(serenity::Error::from(std::io::Error::other(
                    e.to_string(),
                ))))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            tracing::error!("Failed to add Discord reaction {}: {}", status, error_text);
            return Err(BridgeError::Discord(Box::new(serenity::Error::from(
                std::io::Error::other(format!("Discord reaction failed: {status}")),
            ))));
        }

        let count_key = format!("reaction_count|{}|{}", target_event_id, reaction_key);
        let count: u32 = self
            .cache
            .m_messages
            .get(&count_key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        self.cache
            .m_messages
            .insert(count_key, (count + 1).to_string());

        let cache_key =
            format!("reaction|{discord_msg_id}|{target_event_id}|{discord_emoji}|{reaction_key}");

        self.cache
            .m_messages
            .insert(event_id.to_string(), cache_key);

        Ok(())
    }

    async fn handle_typing_event(&self, event: &Value) -> crate::error::Result<()> {
        let room_id = event["room_id"].as_str().unwrap_or("");
        let content = &event["content"];

        let user_ids = content["user_ids"]
            .as_array()
            .ok_or_else(|| BridgeError::Matrix("Typing event missing user_ids".to_string()))?;

        // Check if room is bridged
        let Some(bridge) = self.db.get_bridge(room_id).await? else {
            return Ok(());
        };

        if !bridge.m2d_enabled || !bridge.m2d_typing {
            return Ok(());
        }
        let channel_id = bridge.channel_id;

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
        if let Some(info) = self.cache.d_webhooks.get(channel_id) {
            return Ok(WebhookData {
                id: info.id.clone(),
                token: info.token,
            });
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
                BridgeError::Discord(Box::new(serenity::Error::from(std::io::Error::other(
                    e.to_string(),
                ))))
            })?;

        if !response.status().is_success() {
            return Err(BridgeError::Discord(Box::new(serenity::Error::from(
                std::io::Error::other(format!("Failed to fetch webhooks: {}", response.status())),
            ))));
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
                return Err(BridgeError::Discord(Box::new(serenity::Error::from(
                    std::io::Error::other(format!("Failed to create webhook: {error_text}")),
                ))));
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
        self.cache.d_webhooks.insert(
            channel_id.to_string(),
            crate::cache::WebhookInfo {
                id: webhook_data.id.clone(),
                token: webhook_data.token.clone(),
            },
        );

        Ok(webhook_data)
    }

    async fn send_webhook_message(
        &self,
        webhook: &WebhookData,
        content: &str,
        username: &str,
        avatar_url: Option<&str>,
        thread_id: Option<&str>,
        embed: Option<Value>,
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

        if let Some(e) = embed {
            body["embeds"] = json!([e]);
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
            return Err(BridgeError::Discord(Box::new(serenity::Error::from(
                std::io::Error::other(format!("Webhook send failed {status}: {error_text}")),
            ))));
        }

        let message: Value = response.json().await.unwrap_or_default();
        Ok(message["id"].as_str().unwrap_or("").to_string())
    }

    async fn send_webhook_with_stream(
        &self,
        webhook: &WebhookData,
        content: &str,
        username: &str,
        avatar_url: Option<&str>,
        filename: &str,
        file_body: reqwest::Body,
        embed: Option<Value>,
    ) -> crate::error::Result<String> {
        let url = format!(
            "https://discord.com/api/v10/webhooks/{}/{}?wait=true",
            webhook.id, webhook.token
        );

        let mut payload = json!({
            "username": username,
            "allowed_mentions": {
                "parse": ["users", "roles"]
            }
        });

        if !content.is_empty() {
            payload["content"] = json!(content);
        }

        if let Some(avatar) = avatar_url {
            payload["avatar_url"] = json!(avatar);
        }

        if let Some(e) = embed {
            payload["embeds"] = json!([e]);
        }

        let form = reqwest::multipart::Form::new()
            .text("payload_json", payload.to_string())
            .part(
                "file",
                reqwest::multipart::Part::stream(file_body).file_name(filename.to_string()),
            );

        let response = self
            .discord_http
            .post(&url)
            .multipart(form)
            .send()
            .await
            .map_err(|e| BridgeError::Matrix(format!("Failed to send webhook with file: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            return Err(BridgeError::Discord(Box::new(serenity::Error::from(
                std::io::Error::other(format!("Webhook file send failed {status}: {error_text}")),
            ))));
        }

        let message: Value = response.json().await.unwrap_or_default();
        Ok(message["id"].as_str().unwrap_or("").to_string())
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
            BridgeError::Discord(Box::new(serenity::Error::from(std::io::Error::other(
                e.to_string(),
            ))))
        })?;

        if !response.status().is_success() {
            let status = response.status();

            // 404 means message was already deleted, which is fine
            if status == 404 {
                tracing::debug!("Message {} already deleted", message_id);
                return Ok(());
            }

            let error_text = response.text().await.unwrap_or_default();
            return Err(BridgeError::Discord(Box::new(serenity::Error::from(
                std::io::Error::other(format!("Webhook delete failed {status}: {error_text}")),
            ))));
        }

        Ok(())
    }

    async fn handle_avatar_request(
        &self,
        req: Request<Incoming>,
    ) -> crate::error::Result<Response<Full<Bytes>>> {
        let path = req.uri().path();
        let parts: Vec<&str> = path.split('/').collect();
        if parts.len() < 4 {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from("Invalid path")))
                .unwrap());
        }

        let server_name = parts[2];
        let media_id = parts[3];

        let is_safe = |s: &str| {
            !s.contains("..")
                && s.chars().all(|c| {
                    c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == ':'
                })
        };

        if !is_safe(server_name) || !is_safe(media_id) {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from("Invalid characters in request")))
                .unwrap());
        }

        let query = req.uri().query().unwrap_or("");

        let exp_param = query
            .split('&')
            .find(|s| s.starts_with("exp="))
            .and_then(|s| s.strip_prefix("exp="));

        let Some(exp_str) = exp_param else {
            return Ok(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Full::new(Bytes::from("Missing expiry parameter")))
                .unwrap());
        };

        let Ok(exp_ts) = exp_str.parse::<u64>() else {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from("Invalid expiry format")))
                .unwrap());
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();

        if now > exp_ts {
            tracing::debug!(
                "Rejected expired avatar link for {}/{}",
                server_name,
                media_id
            );
            return Ok(Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Full::new(Bytes::from("Link has expired")))
                .unwrap());
        }

        let sig_param = query
            .split('&')
            .find(|s| s.starts_with("sig="))
            .and_then(|s| s.strip_prefix("sig="));

        let Some(provided_sig_hex) = sig_param else {
            return Ok(Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Full::new(Bytes::from("Missing signature")))
                .unwrap());
        };

        let Ok(provided_sig_bytes) = hex::decode(provided_sig_hex) else {
            return Ok(Response::builder()
                .status(StatusCode::BAD_REQUEST)
                .body(Full::new(Bytes::from("Invalid signature format")))
                .unwrap());
        };

        let mut mac = HmacSha256::new_from_slice(self.config.avatar_proxy_secret.as_bytes())
            .expect("HMAC can take key of any size");

        mac.update(server_name.as_bytes());
        mac.update(b"/");
        mac.update(media_id.as_bytes());
        mac.update(b"?exp=");
        mac.update(exp_str.as_bytes());

        if mac.verify_slice(&provided_sig_bytes).is_err() {
            tracing::warn!(
                "Failed HMAC validation for avatar {}/{}",
                server_name,
                media_id
            );
            return Ok(Response::builder()
                .status(StatusCode::FORBIDDEN)
                .body(Full::new(Bytes::from("Invalid signature")))
                .unwrap());
        }

        let mxc_url = format!("mxc://{server_name}/{media_id}");

        if let Some(data) = self.cache.m_avatars.get(&mxc_url) {
            let content_type = Self::guess_mime_type(&data);
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header("Content-Type", content_type)
                .header("Cache-Control", "public, max-age=86400")
                .body(Full::new(Bytes::from(data)))
                .unwrap());
        }

        match self.matrix.download_media(&mxc_url).await {
            Ok(data) => {
                self.cache.m_avatars.insert(mxc_url, data.clone());

                let content_type = Self::guess_mime_type(&data);

                Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", content_type)
                    .header("Cache-Control", "public, max-age=86400")
                    .body(Full::new(Bytes::from(data)))
                    .unwrap())
            }
            Err(e) => {
                tracing::error!("Failed to fetch avatar for proxy: {}", e);
                Ok(Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Full::new(Bytes::from("Avatar not found")))
                    .unwrap())
            }
        }
    }

    fn guess_mime_type(data: &[u8]) -> &'static str {
        if data.starts_with(b"\x89PNG\r\n\x1a\n") {
            "image/png"
        } else if data.starts_with(b"\xFF\xD8\xFF") {
            "image/jpeg"
        } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
            "image/gif"
        } else if data.starts_with(b"RIFF") && data.len() > 11 && &data[8..12] == b"WEBP" {
            "image/webp"
        } else {
            "application/octet-stream"
        }
    }

    fn strip_matrix_reply_fallback(body: &str) -> String {
        let mut result = String::new();
        let mut in_fallback = true;

        for line in body.lines() {
            if !line.starts_with("> ") && line != ">" {
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

    fn strip_matrix_reply_fallback_html(html: &str) -> String {
        let regex = HTML_REPLY_REGEX
            .get_or_init(|| regex::Regex::new(r"(?s)<mx-reply>.*?</mx-reply>").unwrap());
        regex.replace_all(html, "").to_string()
    }

    async fn find_discord_message_fallback(
        &self,
        channel_id: &str,
        matrix_ts: u64,
        original_body: &str,
    ) -> Option<String> {
        let snowflake = ts_to_snowflake(matrix_ts);

        let url = format!(
            "https://discord.com/api/v10/channels/{}/messages?around={}&limit=20",
            channel_id, snowflake
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
            .ok()?;

        if !response.status().is_success() {
            return None;
        }

        let messages: Vec<Value> = response.json().await.ok()?;

        let target_text = original_body.trim().to_lowercase();
        if target_text.is_empty() {
            return None;
        }

        for msg in messages {
            let msg_content = msg["content"].as_str().unwrap_or("").trim().to_lowercase();

            if msg_content == target_text
                || msg_content.contains(&target_text)
                || target_text.contains(&msg_content)
            {
                return msg["id"].as_str().map(String::from);
            }
        }

        None
    }

    /// Extracts reply information from a Matrix event and builds a Discord embed
    /// linking back to the original message.
    async fn build_reply_embed(
        &self,
        room_id: &str,
        content: &Value,
        members: &HashMap<String, crate::cache::MatrixUser>,
        channel_id: &str,
    ) -> Option<Value> {
        let relates_to = content.get("m.relates_to")?;
        let in_reply_to = relates_to.get("m.in_reply_to")?;
        let reply_event_id = in_reply_to.get("event_id")?.as_str()?;

        let mut discord_reply_msg_id = self.cache.m_messages.get(reply_event_id);

        let mut guild_id = String::from("@me");
        for (g_id, channels) in &self.cache.d_channels {
            if channels.contains_key(channel_id) {
                guild_id = g_id.to_string();
                break;
            }
        }

        let original_event = self.matrix.get_event(room_id, reply_event_id).await.ok()?;

        let original_member = members.get(&original_event.sender);

        let original_name = original_member
            .and_then(|m| m.display_name.as_ref())
            .map_or_else(
                || {
                    original_event
                        .sender
                        .split(':')
                        .next()
                        .unwrap_or(&original_event.sender)
                        .to_string()
                },
                std::clone::Clone::clone,
            );

        let original_avatar_url = original_member
            .and_then(|m| m.avatar_url.as_ref())
            .and_then(|mxc| self.matrix.mxc_to_http(mxc));

        let mut original_body = Self::strip_matrix_reply_fallback(&original_event.body);

        if discord_reply_msg_id.is_none() {
            tracing::info!("Cache miss for reply! Attempting Discord API fallback search...");
            if let Some(found_id) = self
                .find_discord_message_fallback(
                    channel_id,
                    original_event.origin_server_ts,
                    &original_body,
                )
                .await
            {
                tracing::info!("Fallback search succeeded! Found Discord ID: {}", found_id);
                self.cache
                    .insert_message_mapping(reply_event_id.to_string(), found_id.clone());
                discord_reply_msg_id = Some(found_id);
            } else {
                tracing::warn!("Fallback search failed to find matching message.");
            }
        }

        if original_body.trim().is_empty() {
            original_body = "*[Media]*".to_string();
        } else if original_body.len() > 256 {
            original_body.truncate(256);
            original_body.push('…');
        }

        let mut author_obj = json!({
            "name": format!("Replying to {original_name}")
        });

        if let Some(avatar) = original_avatar_url {
            author_obj["icon_url"] = json!(avatar);
        }

        if let Some(msg_id) = discord_reply_msg_id {
            let jump_url = format!("https://discord.com/channels/{guild_id}/{channel_id}/{msg_id}");
            author_obj["url"] = json!(jump_url);
        }

        Some(json!({
            "author": author_obj,
            "description": original_body,
            "color": 0x002b_2d31
        }))
    }

    fn process_matrix_text_for_discord(&self, body: &str, formatted_body: Option<&str>) -> String {
        let process_source = if let Some(html) = formatted_body {
            static SPOILER_REGEX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            let spoiler_regex = SPOILER_REGEX.get_or_init(|| {
                regex::Regex::new(r#"(?s)<span[^>]*data-mx-spoiler[^>]*>(.*?)</span>"#).unwrap()
            });
            let mut preprocessed = spoiler_regex
                .replace_all(html, "\u{E000}$1\u{E001}")
                .to_string();

            static IMG_REGEX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            let img_regex =
                IMG_REGEX.get_or_init(|| regex::Regex::new(r#"(?s)<img[^>]+>"#).unwrap());

            static ALT_REGEX: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            let alt_regex =
                ALT_REGEX.get_or_init(|| regex::Regex::new(r#"alt="([^"]+)""#).unwrap());

            preprocessed = img_regex
                .replace_all(&preprocessed, |caps: &regex::Captures| {
                    let img_tag = caps[0].to_string();
                    if img_tag.contains("data-mx-emoticon") {
                        if let Some(alt_caps) = alt_regex.captures(&img_tag) {
                            let mut alt = alt_caps[1].to_string();

                            if !alt.starts_with(':') {
                                alt = format!(":{alt}");
                            }
                            if !alt.ends_with(':') {
                                alt = format!("{alt}:");
                            }

                            return alt;
                        }
                    }
                    img_tag
                })
                .to_string();

            let mut md = html2md::parse_html(&preprocessed);

            md = md.replace('\u{E000}', "||").replace('\u{E001}', "||");
            md
        } else {
            body.to_string()
        };

        let mention_regex = MENTION_REGEX.get_or_init(|| {
                regex::Regex::new(r"(?s)(```.*?```|`.*?`)|\[[^\]]*\]\(https://matrix\.to/#/@_discord_(\d+)(?:-\d+)?:[\w.\-]+\)|@_discord_(\d+)(?:-\d+)?:[\w.\-]+").unwrap()
            });

        let mentions_replaced =
            mention_regex.replace_all(&process_source, |caps: &regex::Captures| {
                if let Some(code) = caps.get(1) {
                    code.as_str().to_string()
                } else if let Some(id) = caps.get(2).or_else(|| caps.get(3)) {
                    format!("<@{}>", id.as_str())
                } else {
                    caps[0].to_string()
                }
            });

        let emote_regex = EMOTE_REGEX
            .get_or_init(|| regex::Regex::new(r"(?s)(```.*?```|`.*?`)|:(\w+):").unwrap());

        let mut processed_body = emote_regex
            .replace_all(&mentions_replaced, |caps: &regex::Captures| {
                if let Some(code) = caps.get(1) {
                    code.as_str().to_string()
                } else if let Some(emote_name) = caps.get(2) {
                    let name = emote_name.as_str();
                    self.cache
                        .d_emotes
                        .get(name)
                        .unwrap_or_else(|| caps[0].to_string())
                } else {
                    caps[0].to_string()
                }
            })
            .into_owned();

        if processed_body.len() > DISCORD_MESSAGE_LIMIT {
            processed_body.truncate(DISCORD_MESSAGE_LIMIT);
            processed_body.push('…');
        }

        processed_body
    }

    async fn download_mxc_stream(&self, mxc_url: &str) -> crate::error::Result<reqwest::Body> {
        let parts: Vec<&str> = mxc_url.trim_start_matches("mxc://").split('/').collect();
        if parts.len() != 2 {
            return Err(BridgeError::Matrix("Invalid MXC URL format".to_string()));
        }

        let download_url = format!(
            "{}/_matrix/client/v1/media/download/{}/{}",
            self.config.homeserver, parts[0], parts[1]
        );

        let res = self
            .discord_http
            .get(&download_url)
            .header("Authorization", format!("Bearer {}", self.config.as_token))
            .send()
            .await
            .map_err(|e| BridgeError::Matrix(format!("Failed to fetch media stream: {e}")))?;

        if !res.status().is_success() {
            return Err(BridgeError::Matrix(format!(
                "Failed to download media: {}",
                res.status()
            )));
        }

        Ok(reqwest::Body::wrap_stream(res.bytes_stream()))
    }

    fn resolve_sender_profile<'a>(
        &self,
        sender: &'a str,
        members: &'a HashMap<String, crate::cache::MatrixUser>,
    ) -> (&'a str, Option<String>) {
        let member = members.get(sender);

        let display_name = member
            .and_then(|m| m.display_name.as_deref())
            .unwrap_or_else(|| sender.split(':').next().unwrap_or(sender));

        let avatar_url = member
            .and_then(|m| m.avatar_url.as_ref())
            .and_then(|mxc| self.matrix.mxc_to_http(mxc));

        (display_name, avatar_url)
    }
}
