use crate::{cache::Cache, db::Database, matrix_client::MatrixClient, utils::CommandResponse};
use clap::{Parser, Subcommand, ValueEnum};
use ruma::events::room::message::RoomMessageEventContent;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

const USER_HELP_TEMPLATE: &str = "\
    {about}

    {usage-heading} {usage}

    {all-args}

    Additional Standalone Commands:
      !nobridge [text]
              Do not bridge the current message.
      !nobridgefor <duration> [scope]
              Disable bridging temporarily. Duration: 15m, 2h, 1d, etc. Scope: 'here' or 'global' (default: here).
      !neverbridge <scope>
              Permanently disable bridging. Scope: 'here' or 'global'.
      !allowbridge <scope>
              Remove bridging restrictions. Scope: 'here' or 'global'.";

#[derive(Clone, Debug, ValueEnum, PartialEq)]
pub enum Scope {
    Here,
    Global,
}

#[derive(Parser, Debug)]
#[command(
        name = "!bridge",
        about = "Bridge User Commands",
        help_template = USER_HELP_TEMPLATE
    )]
pub struct BridgeCli {
    #[command(subcommand)]
    pub command: BridgeCommand,
}

#[derive(Subcommand, Debug)]
pub enum BridgeCommand {
    Help,
    Status,
}

#[derive(Parser, Debug)]
#[command(name = "!neverbridge", about = "Permanently disable bridging")]
pub struct NeverBridgeCli {
    #[arg(value_enum)]
    pub scope: Scope,
}

#[derive(Parser, Debug)]
#[command(name = "!allowbridge", about = "Allow bridging (removes restrictions)")]
pub struct AllowBridgeCli {
    #[arg(value_enum)]
    pub scope: Scope,
}

#[derive(Parser, Debug)]
#[command(name = "!nobridgefor", about = "Disable bridging temporarily")]
pub struct NoBridgeForCli {
    pub duration: String,
    #[arg(value_enum, default_value_t = Scope::Here)]
    pub scope: Scope,
}

pub struct UserCommandHandler {
    matrix: Arc<MatrixClient>,
    db: Database,
    cache: Cache,
}

impl UserCommandHandler {
    pub const fn new(matrix: Arc<MatrixClient>, db: Database, cache: Cache) -> Self {
        Self { matrix, db, cache }
    }

    pub async fn process_event(
        &self,
        room_id: &str,
        sender: &str,
        body: &str,
    ) -> crate::error::Result<bool> {
        let body_trimmed = body.trim();

        if body_trimmed.starts_with("!nobridge") && !body_trimmed.starts_with("!nobridgefor") {
            return Ok(false);
        }

        if body_trimmed.starts_with("!bridge")
            || body_trimmed.starts_with("!neverbridge")
            || body_trimmed.starts_with("!allowbridge")
            || body_trimmed.starts_with("!nobridgefor")
        {
            let Some(args) = shlex::split(body_trimmed) else {
                let _ = self
                    .send_response(
                        room_id,
                        CommandResponse::Text("Invalid command format.".into()),
                    )
                    .await;
                return Ok(false);
            };

            if body_trimmed.starts_with("!bridge") {
                match BridgeCli::try_parse_from(args) {
                    Ok(cli) => {
                        let resp = self.handle_bridge_cmd(sender, cli.command).await;
                        let _ = self.send_response(room_id, resp).await;
                    }
                    Err(e) => {
                        let _ = self
                            .send_response(room_id, CommandResponse::Terminal(e.to_string()))
                            .await;
                    }
                }
            } else if body_trimmed.starts_with("!neverbridge") {
                match NeverBridgeCli::try_parse_from(args) {
                    Ok(cli) => {
                        let resp = self.handle_neverbridge(room_id, sender, cli.scope).await;
                        let _ = self.send_response(room_id, resp).await;
                    }
                    Err(e) => {
                        let _ = self
                            .send_response(room_id, CommandResponse::Terminal(e.to_string()))
                            .await;
                    }
                }
            } else if body_trimmed.starts_with("!allowbridge") {
                match AllowBridgeCli::try_parse_from(args) {
                    Ok(cli) => {
                        let resp = self.handle_allowbridge(room_id, sender, cli.scope).await;
                        let _ = self.send_response(room_id, resp).await;
                    }
                    Err(e) => {
                        let _ = self
                            .send_response(room_id, CommandResponse::Terminal(e.to_string()))
                            .await;
                    }
                }
            } else if body_trimmed.starts_with("!nobridgefor") {
                match NoBridgeForCli::try_parse_from(args) {
                    Ok(cli) => {
                        let resp = self
                            .handle_nobridgefor(room_id, sender, &cli.duration, cli.scope)
                            .await;
                        let _ = self.send_response(room_id, resp).await;
                    }
                    Err(e) => {
                        let _ = self
                            .send_response(room_id, CommandResponse::Terminal(e.to_string()))
                            .await;
                    }
                }
            }
            return Ok(false);
        }

        Ok(self.should_bridge(room_id, sender).await)
    }

    async fn send_response(
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

    async fn handle_bridge_cmd(&self, sender: &str, cmd: BridgeCommand) -> CommandResponse {
        match cmd {
                BridgeCommand::Help => CommandResponse::Text(
                    "**User Commands**\n\
                    `!nobridge [text]` - Do not bridge this message\n\
                    `!nobridgefor <time> [here|global]` - Disable bridging temporarily (e.g. 15m, 2h, 1d)\n\
                    `!neverbridge <here|global>` - Permanently disable bridging\n\
                    `!allowbridge <here|global>` - Remove bridging restrictions\n\
                    `!bridge status` - View your current bridging configuration".to_string()
                ),
                BridgeCommand::Status => {
                    match self.db.get_user_preferences(sender).await {
                        Ok(prefs) => {
                            if prefs.is_empty() {
                                CommandResponse::Text("You have no explicit bridge configuration. Your messages will be bridged normally.".to_string())
                            } else {
                                let mut rows = Vec::new();
                                for pref in prefs {
                                    let scope = if pref.room_id == "global" { "Global".to_string() } else { pref.room_id.clone() };
                                    let status = match pref.status.as_str() {
                                        "never" => "Never bridge".to_string(),
                                        "until" => {
                                            if let Some(ts) = pref.until_ts {
                                                format!("No bridge until timestamp: {}", ts)
                                            } else {
                                                "Unknown".to_string()
                                            }
                                        },
                                        _ => pref.status.clone(),
                                    };
                                    rows.push(vec![scope, status]);
                                }
                                CommandResponse::Table {
                                    headers: vec!["Scope".to_string(), "Status".to_string()],
                                    rows,
                                    footer: None,
                                }
                            }
                        },
                        Err(e) => CommandResponse::Text(format!("Error fetching status: {}", e)),
                    }
                }
            }
    }

    async fn handle_neverbridge(
        &self,
        room_id: &str,
        sender: &str,
        scope: Scope,
    ) -> CommandResponse {
        let target_room = if scope == Scope::Global {
            "global"
        } else {
            room_id
        };

        match self
            .db
            .set_user_preference(sender, target_room, "never", None)
            .await
        {
            Ok(()) => {
                self.cache
                    .user_prefs
                    .invalidate(&(sender.to_string(), target_room.to_string()));
                let msg = if scope == Scope::Global {
                    "globally"
                } else {
                    "in this room"
                };
                CommandResponse::Text(format!("Bridging disabled {msg}."))
            }
            Err(e) => CommandResponse::Text(format!("Error updating preferences: {}", e)),
        }
    }

    async fn handle_allowbridge(
        &self,
        room_id: &str,
        sender: &str,
        scope: Scope,
    ) -> CommandResponse {
        let target_room = if scope == Scope::Global {
            "global"
        } else {
            room_id
        };

        match self.db.remove_user_preference(sender, target_room).await {
            Ok(()) => {
                self.cache
                    .user_prefs
                    .invalidate(&(sender.to_string(), target_room.to_string()));
                let msg = if scope == Scope::Global {
                    "globally"
                } else {
                    "in this room"
                };
                CommandResponse::Text(format!(
                    "Removed bridging restrictions {msg}. Messages will now be bridged."
                ))
            }
            Err(e) => CommandResponse::Text(format!("Error updating preferences: {}", e)),
        }
    }

    async fn handle_nobridgefor(
        &self,
        room_id: &str,
        sender: &str,
        duration_str: &str,
        scope: Scope,
    ) -> CommandResponse {
        let secs = match Self::parse_duration(duration_str) {
            Some(s) => s,
            None => {
                return CommandResponse::Text(
                    "Invalid time format. Use something like 15m, 2h, 1d, 1M, 1y.".to_string(),
                );
            }
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let until_ts = (now + secs) as i64;

        let target_room = if scope == Scope::Global {
            "global"
        } else {
            room_id
        };

        match self
            .db
            .set_user_preference(sender, target_room, "until", Some(until_ts))
            .await
        {
            Ok(()) => {
                self.cache
                    .user_prefs
                    .invalidate(&(sender.to_string(), target_room.to_string()));
                let msg = if scope == Scope::Global {
                    "globally"
                } else {
                    "in this room"
                };
                CommandResponse::Text(format!("Bridging disabled {msg} for {duration_str}."))
            }
            Err(e) => CommandResponse::Text(format!("Error updating preferences: {}", e)),
        }
    }

    fn parse_duration(s: &str) -> Option<u64> {
        let s = s.replace(" ", "");
        if s.is_empty() {
            return None;
        }

        let idx = s.find(|c: char| !c.is_ascii_digit())?;

        let num_str = &s[..idx];
        let unit = &s[idx..];

        let val: u64 = num_str.parse().ok()?;

        let multiplier = match unit {
            "m" => 60,
            "h" => 60 * 60,
            "d" => 60 * 60 * 24,
            "M" => 60 * 60 * 24 * 30,
            "y" => 60 * 60 * 24 * 365,
            _ => return None,
        };

        Some(val * multiplier)
    }

    async fn should_bridge(&self, room_id: &str, sender: &str) -> bool {
        if !self.check_pref_allows(sender, room_id).await {
            return false;
        }
        if !self.check_pref_allows(sender, "global").await {
            return false;
        }

        true
    }

    async fn check_pref_allows(&self, sender: &str, room_id: &str) -> bool {
        let cache_key = (sender.to_string(), room_id.to_string());

        let pref_opt = if let Some(pref) = self.cache.user_prefs.get(&cache_key) {
            pref
        } else {
            match self.db.get_user_preference(sender, room_id).await {
                Ok(pref) => {
                    self.cache.user_prefs.insert(cache_key, pref.clone());
                    pref
                }
                Err(e) => {
                    tracing::error!("DB error getting user pref: {}", e);
                    None
                }
            }
        };

        if let Some(pref) = pref_opt {
            if pref.status == "never" {
                return false;
            } else if pref.status == "until" {
                if let Some(ts) = pref.until_ts {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_secs() as i64;
                    if now < ts {
                        return false;
                    }
                }
            }
        }
        true
    }
}
