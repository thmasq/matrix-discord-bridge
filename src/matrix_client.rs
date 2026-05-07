use crate::{
    cache::Cache,
    config::Config,
    db::Database,
    discord_client::AttachmentInfo,
    error::{BridgeError, Result},
};
use hmac::KeyInit;
use hmac::{Hmac, Mac};
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request as HyperRequest, Uri, body::Bytes};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use pulldown_cmark::TagEnd;
use pulldown_cmark::{Event, Options, Parser, Tag, html};
use ruma::{OwnedRoomId, events::room::message::RoomMessageEventContent};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{collections::HashMap, sync::OnceLock, time::UNIX_EPOCH};
use std::{fmt::Write, time::SystemTime};

type HmacSha256 = Hmac<Sha256>;

static EMOTE_REGEX: OnceLock<regex::Regex> = OnceLock::new();
static IMG_REGEX: OnceLock<regex::Regex> = OnceLock::new();
static IMG_REGEX_ALT: OnceLock<regex::Regex> = OnceLock::new();
static SHORTCODE_REGEX: OnceLock<regex::Regex> = OnceLock::new();

pub struct MatrixClient {
    config: Config,
    http_client: Client<hyper_tls::HttpsConnector<HttpConnector>, Full<Bytes>>,
    db: Database,
    cache: Cache,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct MatrixEvent {
    pub event_id: String,
    pub sender: String,
    pub body: String,
    pub formatted_body: Option<String>,
}

#[allow(dead_code)]
impl MatrixClient {
    pub fn new(config: Config, db: Database, cache: Cache) -> Self {
        let https = hyper_tls::HttpsConnector::new();
        let http_client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);

        Self {
            config,
            http_client,
            db,
            cache,
        }
    }

    pub async fn send_request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
        user_id: Option<&str>,
    ) -> Result<Value> {
        let mut url = format!("{}/_matrix/client/v3{}", self.config.homeserver, path);

        if let Some(uid) = user_id {
            let _ = write!(url, "?user_id={}", urlencoding::encode(uid));
        }

        let uri: Uri = url.parse().unwrap();

        let body_bytes = if let Some(b) = body {
            serde_json::to_vec(&b)?
        } else {
            Vec::new()
        };

        let req = HyperRequest::builder()
            .method(method)
            .uri(uri)
            .header("Authorization", format!("Bearer {}", self.config.as_token))
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(body_bytes)))
            .unwrap();

        let res = self.http_client.request(req).await?;
        let body = res.collect().await?.to_bytes();

        if body.is_empty() {
            Ok(json!({}))
        } else {
            Ok(serde_json::from_slice(&body)?)
        }
    }

    pub async fn join_room(&self, room_id: &str, mxid: Option<&str>) -> Result<()> {
        self.send_request(
            Method::POST,
            &format!("/join/{}", urlencoding::encode(room_id)),
            None,
            mxid,
        )
        .await?;
        Ok(())
    }

    pub async fn send_message(
        &self,
        room_id: &str,
        content: RoomMessageEventContent,
        mxid: Option<&str>,
    ) -> Result<String> {
        let txn_id = uuid::Uuid::new_v4();
        let resp = self
            .send_request(
                Method::PUT,
                &format!(
                    "/rooms/{}/send/m.room.message/{}",
                    urlencoding::encode(room_id),
                    txn_id
                ),
                Some(serde_json::to_value(&content)?),
                mxid,
            )
            .await?;

        Ok(resp["event_id"].as_str().unwrap().to_string())
    }

    pub async fn send_invite(&self, room_id: &str, mxid: &str) -> Result<()> {
        self.send_request(
            Method::POST,
            &format!("/rooms/{}/invite", urlencoding::encode(room_id)),
            Some(json!({ "user_id": mxid })),
            None,
        )
        .await?;
        Ok(())
    }

    pub async fn create_room(
        &self,
        channel_id: &str,
        name: &str,
        topic: &str,
        invitee: &str,
    ) -> Result<OwnedRoomId> {
        let _room_alias = format!("_discord_{}:{}", channel_id, self.config.server_name);

        let content = json!({
            "room_alias_name": format!("_discord_{}", channel_id),
            "name": name,
            "topic": topic,
            "visibility": "private",
            "invite": [invitee],
            "creation_content": { "m.federate": true },
            "initial_state": [
                {
                    "type": "m.room.join_rules",
                    "content": { "join_rule": "public" }
                },
                {
                    "type": "m.room.history_visibility",
                    "content": { "history_visibility": "shared" }
                }
            ],
            "power_level_content_override": {
                "users": {
                    invitee: 100,
                    self.config.full_user_id(): 100
                }
            }
        });

        let resp = self
            .send_request(Method::POST, "/createRoom", Some(content), None)
            .await?;
        let room_id = resp["room_id"].as_str().unwrap().to_string();

        self.db.add_room(&room_id, channel_id).await?;

        Ok(room_id.try_into().unwrap())
    }

    pub async fn register_user(&self, mxid: &str) -> Result<()> {
        let username = mxid.trim_start_matches('@').split(':').next().unwrap();

        let content = json!({
            "type": "m.login.application_service",
            "username": username
        });

        self.send_request(Method::POST, "/register", Some(content), None)
            .await?;
        self.db.add_user(mxid).await?;

        Ok(())
    }

    pub async fn set_display_name(&self, mxid: &str, display_name: &str) -> Result<()> {
        self.send_request(
            Method::PUT,
            &format!("/profile/{}/displayname", urlencoding::encode(mxid)),
            Some(json!({ "displayname": display_name })),
            Some(mxid),
        )
        .await?;

        self.db.update_username(mxid, display_name).await?;
        Ok(())
    }

    pub async fn set_avatar(&self, mxid: &str, avatar_url: &str) -> Result<()> {
        let mxc_url = self.upload_from_url(avatar_url).await?;

        self.send_request(
            Method::PUT,
            &format!("/profile/{}/avatar_url", urlencoding::encode(mxid)),
            Some(json!({ "avatar_url": mxc_url })),
            Some(mxid),
        )
        .await?;

        self.db.update_avatar(mxid, avatar_url).await?;
        Ok(())
    }

    pub async fn upload_from_url(&self, url: &str) -> Result<String> {
        let uri: hyper::Uri = url
            .parse()
            .map_err(|e| BridgeError::Matrix(format!("Invalid avatar URL '{url}': {e}")))?;

        if !Self::is_trusted_discord_url(&uri) {
            tracing::warn!("Blocked attempt to download from untrusted URL: {}", url);
            return Err(BridgeError::Matrix(
                "Refused to fetch from untrusted domain".to_string(),
            ));
        }

        let req = HyperRequest::builder()
            .method(Method::GET)
            .uri(uri)
            .body(Full::new(Bytes::new()))
            .unwrap();

        let res = self.http_client.request(req).await?;

        if res.status() != hyper::StatusCode::OK {
            return Err(BridgeError::Matrix(format!(
                "Failed to download media from URL: {}",
                res.status()
            )));
        }

        let bytes = res.collect().await?.to_bytes();

        let content_type = if std::path::Path::new(url)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("gif"))
        {
            "image/gif"
        } else if std::path::Path::new(url)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("webp"))
        {
            "image/webp"
        } else if std::path::Path::new(url)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg"))
        {
            "image/jpeg"
        } else {
            "image/png"
        };

        let upload_url = format!("{}/_matrix/media/v3/upload", self.config.homeserver);
        let upload_req = HyperRequest::builder()
            .method(Method::POST)
            .uri(upload_url)
            .header("Authorization", format!("Bearer {}", self.config.as_token))
            .header("Content-Type", content_type)
            .body(Full::new(bytes))
            .unwrap();

        let res = self.http_client.request(upload_req).await?;

        if res.status() != hyper::StatusCode::OK {
            return Err(BridgeError::Matrix(format!(
                "Failed to upload media: {}",
                res.status()
            )));
        }

        let body = res.collect().await?.to_bytes();
        let json: Value = serde_json::from_slice(&body)?;

        Ok(json["content_uri"].as_str().unwrap().to_string())
    }

    pub fn mxc_to_http(&self, mxc: &str) -> Option<String> {
        if !mxc.starts_with("mxc://") {
            return None;
        }

        let parts: Vec<&str> = mxc.trim_start_matches("mxc://").split('/').collect();
        if parts.len() != 2 {
            return None;
        }

        let server_name = parts[0];
        let media_id = parts[1];

        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + (86400 * 2); // 2 days

        let exp_str = exp.to_string();

        let mut mac = HmacSha256::new_from_slice(self.config.avatar_proxy_secret.as_bytes())
            .expect("HMAC can take key of any size");

        mac.update(server_name.as_bytes());
        mac.update(b"/");
        mac.update(media_id.as_bytes());
        mac.update(b"?exp=");
        mac.update(exp_str.as_bytes());

        let signature = hex::encode(mac.finalize().into_bytes());

        Some(format!(
            "{}/avatar/{}/{}?exp={}&sig={}",
            self.config.avatar_public_url.trim_end_matches('/'),
            server_name,
            media_id,
            exp_str,
            signature
        ))
    }

    pub fn matrixify_user(&self, discord_id: &str, hashed: Option<&str>) -> String {
        format!(
            "@_discord_{}{}:{}",
            discord_id,
            hashed.map(|h| format!("-{h}")).unwrap_or_default(),
            self.config.server_name
        )
    }

    pub fn matrixify_room(&self, discord_channel_id: &str) -> String {
        format!(
            "#_discord_{}:{}",
            discord_channel_id, self.config.server_name
        )
    }
    pub async fn resolve_room_alias(&self, alias: &str) -> Result<String> {
        // Check cache first
        if let Some(room_id) = self.cache.m_rooms.get(alias) {
            return Ok(room_id);
        }

        // Query homeserver
        let resp = self
            .send_request(
                Method::GET,
                &format!("/directory/room/{}", urlencoding::encode(alias)),
                None,
                None,
            )
            .await?;

        let room_id = resp["room_id"]
            .as_str()
            .ok_or_else(|| BridgeError::Matrix("No room_id in response".into()))?
            .to_string();

        // Cache it
        self.cache
            .m_rooms
            .insert(alias.to_string(), room_id.clone());

        Ok(room_id)
    }

    pub async fn redact_event(
        &self,
        room_id: &str,
        event_id: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let txn_id = uuid::Uuid::new_v4();
        let mut content = json!({});

        if let Some(r) = reason {
            content["reason"] = json!(r);
        }

        self.send_request(
            Method::PUT,
            &format!(
                "/rooms/{}/redact/{}/{}",
                urlencoding::encode(room_id),
                urlencoding::encode(event_id),
                txn_id
            ),
            Some(content),
            None,
        )
        .await?;

        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    pub async fn process_for_matrix(
        &self,
        message: &str,
        mxc_emotes: &HashMap<String, String>,
    ) -> (String, String) {
        let mut resolved_emotes = HashMap::new();
        let emote_regex =
            EMOTE_REGEX.get_or_init(|| regex::Regex::new(r":([a-zA-Z0-9_-]+):").unwrap());

        for cap in emote_regex.captures_iter(message) {
            let emote_name = cap.get(1).unwrap().as_str();

            if let Some(mxc_url) = mxc_emotes.get(emote_name) {
                resolved_emotes.insert(emote_name.to_string(), mxc_url.clone());
            }
        }

        let mut options = Options::empty();
        options.insert(Options::ENABLE_STRIKETHROUGH);
        options.insert(Options::ENABLE_TABLES);

        let mut in_code_block = false;
        let mut in_spoiler = false;

        let parser = Parser::new_ext(message, options).flat_map(|event| {
            let mut events = Vec::new();

            match event {
                Event::Start(Tag::CodeBlock(_)) => {
                    in_code_block = true;
                    events.push(event);
                }
                Event::End(TagEnd::CodeBlock) => {
                    in_code_block = false;
                    events.push(event);
                }
                Event::Code(_) => {
                    events.push(event);
                }
                Event::SoftBreak => {
                    events.push(Event::HardBreak);
                }
                Event::Text(text) if !in_code_block => {
                    for (i, part) in text.split("||").enumerate() {
                        if i > 0 {
                            if in_spoiler {
                                events.push(Event::Html("</span>".into()));
                                in_spoiler = false;
                            } else {
                                events.push(Event::Html("<span data-mx-spoiler>".into()));
                                in_spoiler = true;
                            }
                        }

                        if !part.is_empty() {
                            let mut last_end = 0;
                            for cap in emote_regex.captures_iter(part) {
                                let mat = cap.get(0).unwrap();
                                let emote_name = cap.get(1).unwrap().as_str();

                                if let Some(mxc_url) = resolved_emotes.get(emote_name) {
                                    if mat.start() > last_end {
                                        events.push(Event::Text(part[last_end..mat.start()].to_string().into()));
                                    }
                                    let emote_html = format!(
                                        r#"<img alt=":{emote_name}:" title=":{emote_name}:" height="32" src="{mxc_url}" data-mx-emoticon />"#
                                    );
                                    events.push(Event::Html(emote_html.into()));
                                    last_end = mat.end();
                                }
                            }
                            if last_end < part.len() {
                                events.push(Event::Text(part[last_end..].to_string().into()));
                            }
                        }
                    }
                }
                _ => events.push(event),
            }
            events.into_iter()
        });

        let mut html_output = String::new();
        html::push_html(&mut html_output, parser);

        if in_spoiler {
            html_output.push_str("</span>");
        }

        let mut formatted = html_output.trim().to_string();
        if formatted.starts_with("<p>")
            && formatted.ends_with("</p>")
            && formatted.matches("<p>").count() == 1
        {
            formatted = formatted
                .strip_prefix("<p>")
                .unwrap()
                .strip_suffix("</p>")
                .unwrap()
                .trim()
                .to_string();
        }

        (message.to_string(), formatted)
    }

    pub async fn send_typing(&self, room_id: &str, mxid: &str, timeout_ms: u32) -> Result<()> {
        self.send_request(
            Method::PUT,
            &format!(
                "/rooms/{}/typing/{}",
                urlencoding::encode(room_id),
                urlencoding::encode(mxid)
            ),
            Some(json!({
                "typing": true,
                "timeout": timeout_ms
            })),
            Some(mxid),
        )
        .await?;
        Ok(())
    }

    pub async fn send_edit(
        &self,
        room_id: &str,
        original_event_id: &str,
        new_content: RoomMessageEventContent,
        mxid: Option<&str>,
    ) -> Result<String> {
        // Serialize the new content
        let mut content_json = serde_json::to_value(&new_content)?;

        // Add m.new_content field with the actual new content
        let new_content_body = content_json.clone();
        content_json["m.new_content"] = new_content_body;

        // Add m.relates_to for the edit relationship
        content_json["m.relates_to"] = json!({
            "rel_type": "m.replace",
            "event_id": original_event_id
        });

        // The body should indicate this is an edit with fallback text
        // for clients that don't support edits
        if let Some(body) = content_json["body"].as_str() {
            content_json["body"] = json!(format!("* {}", body));
        }
        if let Some(formatted_body) = content_json["formatted_body"].as_str() {
            content_json["formatted_body"] = json!(format!("* {}", formatted_body));
        }

        let txn_id = uuid::Uuid::new_v4();
        let resp = self
            .send_request(
                Method::PUT,
                &format!(
                    "/rooms/{}/send/m.room.message/{}",
                    urlencoding::encode(room_id),
                    txn_id
                ),
                Some(content_json),
                mxid,
            )
            .await?;

        Ok(resp["event_id"].as_str().unwrap().to_string())
    }

    pub async fn send_reaction(
        &self,
        room_id: &str,
        event_id: &str,
        reaction_key: &str,
        mxid: &str,
    ) -> Result<String> {
        let content = json!({
            "m.relates_to": {
                "rel_type": "m.annotation",
                "event_id": event_id,
                "key": reaction_key
            }
        });

        let txn_id = uuid::Uuid::new_v4();
        let resp = self
            .send_request(
                Method::PUT,
                &format!(
                    "/rooms/{}/send/m.reaction/{}",
                    urlencoding::encode(room_id),
                    txn_id
                ),
                Some(content),
                Some(mxid),
            )
            .await?;

        Ok(resp["event_id"].as_str().unwrap().to_string())
    }
    pub async fn send_media(
        &self,
        room_id: &str,
        mxid: &str,
        attachment: &AttachmentInfo,
    ) -> Result<String> {
        let uri: hyper::Uri = attachment
            .url
            .parse()
            .map_err(|_| BridgeError::Matrix("Invalid attachment URL format".to_string()))?;

        if !Self::is_trusted_discord_url(&uri) {
            tracing::warn!(
                "Blocked attempt to download attachment from untrusted URL: {}",
                attachment.url
            );
            return Err(BridgeError::Matrix(
                "Refused to fetch from untrusted domain".to_string(),
            ));
        }

        let http_client = reqwest::Client::new();

        let res = http_client.get(&attachment.url).send().await.map_err(|e| {
            BridgeError::Matrix(format!("Failed to fetch attachment from Discord: {e}"))
        })?;

        let file_body = reqwest::Body::wrap_stream(res.bytes_stream());

        let upload_url = format!("{}/_matrix/media/v3/upload", self.config.homeserver);

        let content_type = attachment
            .content_type
            .as_deref()
            .unwrap_or("application/octet-stream");

        let upload_res = http_client
            .post(&upload_url)
            .header("Authorization", format!("Bearer {}", self.config.as_token))
            .header("Content-Type", content_type)
            .header("Content-Length", attachment.size.to_string())
            .body(file_body)
            .send()
            .await
            .map_err(|e| BridgeError::Matrix(format!("Upload failed: {e}")))?;

        if !upload_res.status().is_success() {
            return Err(BridgeError::Matrix(format!(
                "Failed to upload attachment: {}",
                upload_res.status()
            )));
        }

        let upload_resp: Value = upload_res
            .json()
            .await
            .map_err(|e| BridgeError::Matrix(format!("Failed to parse upload response: {e}")))?;

        let mxc_url = upload_resp["content_uri"].as_str().unwrap();

        // Determine message type based on content type
        let (msgtype, extra_info) =
            Self::determine_media_type(attachment, mxc_url, attachment.size as usize);

        let mut content = json!({
            "msgtype": msgtype,
            "body": attachment.filename,
            "url": mxc_url,
            "info": extra_info
        });

        if attachment.filename.starts_with("SPOILER_")
            || attachment.filename.starts_with("spoiler_")
        {
            content["page.codeberg.everypizza.msc4193.spoiler"] = json!(true);
        }

        let txn_id = uuid::Uuid::new_v4();
        let resp = self
            .send_request(
                Method::PUT,
                &format!(
                    "/rooms/{}/send/m.room.message/{}",
                    urlencoding::encode(room_id),
                    txn_id
                ),
                Some(content),
                Some(mxid),
            )
            .await?;

        Ok(resp["event_id"].as_str().unwrap().to_string())
    }

    fn determine_media_type(
        attachment: &AttachmentInfo,
        mxc_url: &str,
        size: usize,
    ) -> (String, Value) {
        let content_type = attachment
            .content_type
            .as_deref()
            .unwrap_or("application/octet-stream");

        let mut info = json!({
            "size": size,
            "mimetype": content_type
        });

        let msgtype = if content_type.starts_with("image/") {
            if let (Some(w), Some(h)) = (attachment.width, attachment.height) {
                info["w"] = json!(w);
                info["h"] = json!(h);

                // Generate thumbnail URL (same as original for now)
                info["thumbnail_url"] = json!(mxc_url);
                info["thumbnail_info"] = json!({
                    "mimetype": content_type,
                    "size": size,
                    "w": w,
                    "h": h
                });
            }
            "m.image"
        } else if content_type.starts_with("video/") {
            if let (Some(w), Some(h)) = (attachment.width, attachment.height) {
                info["w"] = json!(w);
                info["h"] = json!(h);
            }
            "m.video"
        } else if content_type.starts_with("audio/") {
            "m.audio"
        } else {
            "m.file"
        };

        (msgtype.to_string(), info)
    }

    pub async fn download_media(&self, mxc_url: &str) -> Result<Vec<u8>> {
        if !mxc_url.starts_with("mxc://") {
            return Err(BridgeError::Matrix("Invalid MXC URL".to_string()));
        }

        let parts: Vec<&str> = mxc_url.trim_start_matches("mxc://").split('/').collect();
        if parts.len() != 2 {
            return Err(BridgeError::Matrix("Invalid MXC URL format".to_string()));
        }

        let download_url = format!(
            "{}/_matrix/client/v1/media/download/{}/{}",
            self.config.homeserver, parts[0], parts[1]
        );

        let uri: Uri = download_url.parse().unwrap();
        let req = HyperRequest::builder()
            .uri(uri)
            .header("Authorization", format!("Bearer {}", self.config.as_token))
            .body(Full::new(Bytes::new()))
            .unwrap();

        let res = self.http_client.request(req).await?;

        if res.status() != hyper::StatusCode::OK {
            return Err(BridgeError::Matrix(format!(
                "Failed to download media: {}",
                res.status()
            )));
        }

        let bytes = res.collect().await?.to_bytes();
        Ok(bytes.to_vec())
    }

    /// Fetch custom emoji (image packs) for a room
    /// Supports MSC2545 (`im.ponies.emote_rooms`) used by Nheko, Cinny, etc.
    pub async fn fetch_room_emojis(&self, room_id: &str) -> Result<HashMap<String, String>> {
        let mut emojis = HashMap::new();

        // Try to get im.ponies.emote_rooms state event
        let resp = self
            .send_request(
                Method::GET,
                &format!(
                    "/rooms/{}/state/im.ponies.emote_rooms",
                    urlencoding::encode(room_id)
                ),
                None,
                None,
            )
            .await;

        if let Ok(state_event) = resp {
            // Parse the emote rooms
            if let Some(rooms) = state_event["rooms"].as_object() {
                for (_room_key, room_data) in rooms {
                    if let Some(images) = room_data["images"].as_object() {
                        for (shortcode, image_data) in images {
                            if let Some(url) = image_data["url"].as_str() {
                                emojis.insert(shortcode.clone(), url.to_string());
                            }
                        }
                    }
                }
            }
        }

        // Also try im.ponies.room_emotes (alternative format)
        let resp = self
            .send_request(
                Method::GET,
                &format!(
                    "/rooms/{}/state/im.ponies.room_emotes",
                    urlencoding::encode(room_id)
                ),
                None,
                None,
            )
            .await;

        if let Ok(state_event) = resp
            && let Some(images) = state_event["images"].as_object()
        {
            for (shortcode, image_data) in images {
                if let Some(url) = image_data["url"].as_str() {
                    emojis.insert(shortcode.clone(), url.to_string());
                }
            }
        }

        // Cache the results
        if !emojis.is_empty() {
            self.cache
                .m_custom_emojis
                .insert(room_id.to_string(), emojis.clone());
        }

        Ok(emojis)
    }

    /// Parse Matrix message and extract custom emoji usage
    /// Returns `emoji_map` where `emoji_map` is shortcode -> MXC URL
    pub fn parse_matrix_emojis(
        body: &str,
        formatted_body: Option<&str>,
    ) -> HashMap<String, String> {
        let mut emojis = HashMap::new();

        // Parse HTML formatted body for <img data-mx-emoticon> tags
        if let Some(html) = formatted_body {
            let img_regex = IMG_REGEX.get_or_init(|| regex::Regex::new(
                r#"<img[^>]*data-mx-emoticon[^>]*src="(mxc://[^"]+)"[^>]*(?:alt|title)="?:?([^:">]+):?"?[^>]*/?>"#
            ).unwrap());

            for cap in img_regex.captures_iter(html) {
                if let (Some(mxc_url), Some(shortcode)) = (cap.get(1), cap.get(2)) {
                    emojis.insert(shortcode.as_str().to_string(), mxc_url.as_str().to_string());
                }
            }

            // Also try reversed order (title before src)
            let img_regex_alt = IMG_REGEX_ALT.get_or_init(|| regex::Regex::new(
                r#"<img[^>]*(?:alt|title)="?:?([^:">]+):?"?[^>]*data-mx-emoticon[^>]*src="(mxc://[^"]+)"[^>]*/?>"#
            ).unwrap());

            for cap in img_regex_alt.captures_iter(html) {
                if let (Some(shortcode), Some(mxc_url)) = (cap.get(1), cap.get(2)) {
                    emojis.insert(shortcode.as_str().to_string(), mxc_url.as_str().to_string());
                }
            }
        }

        // If no HTML, look for :shortcode: patterns in plain text
        if emojis.is_empty() {
            let shortcode_regex =
                SHORTCODE_REGEX.get_or_init(|| regex::Regex::new(r":([a-zA-Z0-9_-]+):").unwrap());

            for cap in shortcode_regex.captures_iter(body) {
                if let Some(shortcode) = cap.get(1) {
                    // Mark it as found but without MXC URL
                    // We'll need to look it up from room emojis
                    emojis.insert(shortcode.as_str().to_string(), String::new());
                }
            }
        }

        emojis
    }

    /// Get cached custom emojis for a room, or fetch if not cached
    pub async fn get_room_emojis(&self, room_id: &str) -> Result<HashMap<String, String>> {
        // Check cache first
        if let Some(emojis) = self.cache.m_custom_emojis.get(room_id) {
            return Ok(emojis);
        }

        // Fetch and cache
        self.fetch_room_emojis(room_id).await
    }

    pub async fn send_sticker(
        &self,
        room_id: &str,
        mxid: &str,
        sticker_url: &str,
        filename: &str,
    ) -> Result<String> {
        // Download the sticker
        let uri: Uri = sticker_url.parse().unwrap();

        if !Self::is_trusted_discord_url(&uri) {
            tracing::warn!(
                "Blocked attempt to download sticker from untrusted URL: {}",
                sticker_url
            );
            return Err(BridgeError::Matrix(
                "Refused to fetch from untrusted domain".to_string(),
            ));
        }

        let req = HyperRequest::builder()
            .uri(uri)
            .body(Full::new(Bytes::new()))
            .unwrap();

        let res = self.http_client.request(req).await?;
        let bytes = res.collect().await?.to_bytes();

        // Determine content type from URL or default to PNG
        let content_type = if std::path::Path::new(sticker_url)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("gif"))
        {
            "image/gif"
        } else if std::path::Path::new(sticker_url)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("webp"))
        {
            "image/webp"
        } else {
            "image/png"
        };

        let upload_url = format!("{}/_matrix/media/v3/upload", self.config.homeserver);
        let upload_req = HyperRequest::builder()
            .method(Method::POST)
            .uri(upload_url)
            .header("Authorization", format!("Bearer {}", self.config.as_token))
            .header("Content-Type", content_type)
            .body(Full::new(bytes.clone()))
            .unwrap();

        let res = self.http_client.request(upload_req).await?;

        if res.status() != hyper::StatusCode::OK {
            return Err(BridgeError::Matrix(format!(
                "Failed to upload sticker: {}",
                res.status()
            )));
        }

        let body = res.collect().await?.to_bytes();
        let upload_resp: Value = serde_json::from_slice(&body)?;
        let mxc_url = upload_resp["content_uri"].as_str().unwrap();

        // Try to extract dimensions if possible (Discord stickers are typically 320x320)
        let info = json!({
            "mimetype": content_type,
            "size": bytes.len(),
            "w": 320,
            "h": 320
        });

        // Create sticker content
        let content = json!({
            "body": filename,
            "url": mxc_url,
            "info": info
        });

        // Send as m.sticker event
        let txn_id = uuid::Uuid::new_v4();
        let resp = self
            .send_request(
                Method::PUT,
                &format!(
                    "/rooms/{}/send/m.sticker/{}",
                    urlencoding::encode(room_id),
                    txn_id
                ),
                Some(content),
                Some(mxid),
            )
            .await?;

        Ok(resp["event_id"].as_str().unwrap().to_string())
    }

    pub async fn is_user_in_room(&self, room_id: &str, mxid: &str) -> Result<bool> {
        // First check the cache
        if let Some(room_members) = self.cache.m_members.get(room_id)
            && room_members.contains_key(mxid)
        {
            return Ok(true);
        }

        // If not in cache, query the homeserver
        let Ok(resp) = self
            .send_request(
                Method::GET,
                &format!("/rooms/{}/joined_members", urlencoding::encode(room_id)),
                None,
                None,
            )
            .await
        else {
            // Room might not exist or we don't have access
            return Ok(false);
        };

        // Check if the user is in the joined members
        resp["joined"]
            .as_object()
            .map_or_else(|| Ok(false), |joined| Ok(joined.contains_key(mxid)))
    }

    pub async fn get_event(&self, room_id: &str, event_id: &str) -> Result<MatrixEvent> {
        let resp = self
            .send_request(
                Method::GET,
                &format!(
                    "/rooms/{}/event/{}",
                    urlencoding::encode(room_id),
                    urlencoding::encode(event_id)
                ),
                None,
                None,
            )
            .await?;

        let empty = serde_json::json!({});
        let content = resp.get("content").unwrap_or(&empty);

        Ok(MatrixEvent {
            event_id: resp["event_id"].as_str().unwrap_or("").to_string(),
            sender: resp["sender"].as_str().unwrap_or("").to_string(),
            body: content["body"].as_str().unwrap_or("").to_string(),
            formatted_body: content["formatted_body"].as_str().map(String::from),
        })
    }

    pub async fn leave_room(&self, room_id: &str, mxid: Option<&str>) -> Result<()> {
        self.send_request(
            Method::POST,
            &format!("/rooms/{}/leave", urlencoding::encode(room_id)),
            Some(json!({})),
            mxid,
        )
        .await?;
        Ok(())
    }

    /// Validates that a given URI points to a trusted Discord media domain
    fn is_trusted_discord_url(uri: &hyper::Uri) -> bool {
        if uri.scheme_str() != Some("https") {
            return false;
        }

        uri.host()
            .is_some_and(|host| host == "cdn.discordapp.com" || host == "media.discordapp.net")
    }
}
