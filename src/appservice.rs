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
use ruma::events::{
    AnyTimelineEvent,
    room::{
        member::RoomMemberEventContent, message::RoomMessageEventContent,
        redaction::RoomRedactionEventContent,
    },
};
use serde_json::Value;
use std::sync::Arc;
use tokio::net::TcpListener;

pub struct AppService {
    config: Config,
    matrix: Arc<MatrixClient>,
    db: Database,
    cache: Cache,
}

impl AppService {
    pub fn new(config: Config, matrix: Arc<MatrixClient>, db: Database, cache: Cache) -> Self {
        Self {
            config,
            matrix,
            db,
            cache,
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
        let sender = event["sender"].as_str().unwrap();
        let state_key = event["state_key"].as_str().unwrap();
        let content = &event["content"];

        // Clear member cache for this room
        self.cache.m_members.write().remove(room_id);

        // If it's an invite to our bot user in a DM, join
        if state_key == self.config.full_user_id() {
            if let Some(true) = content["is_direct"].as_bool() {
                tracing::info!("Joining DM room {}", room_id);
                self.matrix.join_room(room_id, None).await?;
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

        // Ignore our own messages
        if sender.starts_with("@_discord_") || sender == self.config.full_user_id() {
            return Ok(());
        }

        // Handle bridge commands
        if body.starts_with("!bridge ") {
            let parts: Vec<&str> = body.split_whitespace().collect();
            if parts.len() >= 2 {
                let channel_id = parts[1];
                tracing::info!("Bridge command for channel {}", channel_id);
                // In full implementation: validate channel, create room, etc.
            }
            return Ok(());
        }

        // Check if this room is bridged
        if let Some(channel_id) = self.db.get_channel(room_id).await? {
            // Forward to Discord
            tracing::info!(
                "Forwarding message from {} to Discord channel {}",
                sender,
                channel_id
            );
            // In full implementation: get webhook, send message
        }

        Ok(())
    }

    async fn handle_redaction_event(&self, event: &Value) -> crate::error::Result<()> {
        let room_id = event["room_id"].as_str().unwrap();
        let redacts = event["redacts"].as_str().unwrap();

        // Look up the Discord message ID from cache
        let message_id = self.cache.m_messages.read().get(redacts).cloned();

        if let Some(msg_id) = message_id {
            tracing::info!("Redacting message {}", msg_id);
            // In full implementation: delete Discord message via webhook
            self.cache.m_messages.write().remove(redacts);
        }

        Ok(())
    }
}
