use crate::{cache::Cache, config::Config, db::Database, error::Result};
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request as HyperRequest, Uri, body::Bytes};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use ruma::{
    OwnedRoomId, OwnedUserId, RoomAliasId, RoomId, UserId,
    api::client::room::create_room::v3::Request as CreateRoomRequest,
    events::room::message::RoomMessageEventContent,
};
use serde_json::{Value, json};
use std::sync::Arc;

pub struct MatrixClient {
    config: Config,
    http_client: Client<HttpConnector, Full<Bytes>>,
    db: Database,
    cache: Cache,
}

impl MatrixClient {
    pub fn new(config: Config, db: Database, cache: Cache) -> Self {
        let http_client = Client::builder(hyper_util::rt::TokioExecutor::new()).build_http();

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
        let mut url = format!("{}/_matrix/client/r0{}", self.config.homeserver, path);

        if let Some(uid) = user_id {
            url.push_str(&format!("?user_id={}", urlencoding::encode(uid)));
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
        let room_alias = format!("_discord_{}:{}", channel_id, self.config.server_name);

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
        // Download from URL
        let uri: Uri = url.parse().unwrap();
        let req = HyperRequest::builder()
            .uri(uri)
            .body(Full::new(Bytes::new()))
            .unwrap();

        let res = self.http_client.request(req).await?;
        let bytes = res.collect().await?.to_bytes();

        // Upload to homeserver
        let upload_url = format!("{}/_matrix/media/r0/upload", self.config.homeserver);
        let req = HyperRequest::builder()
            .method(Method::POST)
            .uri(upload_url)
            .header("Authorization", format!("Bearer {}", self.config.as_token))
            .header("Content-Type", "application/octet-stream")
            .body(Full::new(bytes))
            .unwrap();

        let res = self.http_client.request(req).await?;
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

        Some(format!(
            "https://{}/_matrix/media/r0/download/{}/{}",
            self.config.server_name, parts[0], parts[1]
        ))
    }

    pub fn matrixify_user(&self, discord_id: &str, hashed: Option<&str>) -> String {
        format!(
            "@_discord_{}{}:{}",
            discord_id,
            hashed.map(|h| format!("-{}", h)).unwrap_or_default(),
            self.config.server_name
        )
    }

    pub fn matrixify_room(&self, discord_channel_id: &str) -> String {
        format!(
            "#_discord_{}:{}",
            discord_channel_id, self.config.server_name
        )
    }
}
