use sqlx::{
    SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::path::Path;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct Database {
    pool: SqlitePool,
}

#[derive(Debug, sqlx::FromRow)]
#[allow(clippy::struct_excessive_bools)]
pub struct BridgedRoom {
    pub room_id: String,
    pub channel_id: String,
    pub d2m_enabled: bool,
    pub m2d_enabled: bool,
    pub d2m_mod_deletions: bool,
    pub m2d_mod_deletions: bool,
    pub d2m_typing: bool,
    pub m2d_typing: bool,
}

#[derive(Debug, sqlx::FromRow)]
#[allow(dead_code)]
pub struct BridgedUser {
    pub mxid: String,
    pub avatar_url: Option<String>,
    pub username: Option<String>,
}

#[derive(Debug, sqlx::FromRow)]
pub struct PendingInvite {
    pub id: i64,
    pub room_id: String,
    pub sender: String,
    pub room_name: Option<String>,
}

impl Database {
    pub async fn new(path: impl AsRef<Path>) -> crate::error::Result<Self> {
        let url = format!("sqlite:{}", path.as_ref().display());

        let options = SqliteConnectOptions::from_str(&url)?
            .pragma("journal_mode", "WAL")
            .pragma("synchronous", "NORMAL")
            .pragma("foreign_keys", "ON");

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await?;

        // Create tables
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS bridge (
                room_id TEXT PRIMARY KEY,
                channel_id TEXT NOT NULL,
                d2m_enabled BOOLEAN NOT NULL DEFAULT 1,
                m2d_enabled BOOLEAN NOT NULL DEFAULT 1,
                d2m_mod_deletions BOOLEAN NOT NULL DEFAULT 0,
                m2d_mod_deletions BOOLEAN NOT NULL DEFAULT 0,
                d2m_typing BOOLEAN NOT NULL DEFAULT 1,
                m2d_typing BOOLEAN NOT NULL DEFAULT 1
            )",
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_bridge_channel_id ON bridge(channel_id)",
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS users (
                mxid TEXT PRIMARY KEY,
                avatar_url TEXT,
                username TEXT
            )",
        )
        .execute(&pool)
        .await?;

        sqlx::query(
            "CREATE TABLE IF NOT EXISTS invites (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                room_id TEXT NOT NULL UNIQUE,
                sender TEXT NOT NULL,
                room_name TEXT
            )",
        )
        .execute(&pool)
        .await?;

        Ok(Self { pool })
    }

    pub async fn add_room(&self, room_id: &str, channel_id: &str) -> crate::error::Result<()> {
        sqlx::query("INSERT INTO bridge (room_id, channel_id, d2m_enabled, m2d_enabled, d2m_mod_deletions, m2d_mod_deletions, d2m_typing, m2d_typing) VALUES (?, ?, 1, 1, 0, 0, 1, 1)")
            .bind(room_id)
            .bind(channel_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_channel(&self, room_id: &str) -> crate::error::Result<Option<String>> {
        let result =
            sqlx::query_scalar::<_, String>("SELECT channel_id FROM bridge WHERE room_id = ?")
                .bind(room_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(result)
    }

    pub async fn get_room_by_channel(
        &self,
        channel_id: &str,
    ) -> crate::error::Result<Option<String>> {
        let result =
            sqlx::query_scalar::<_, String>("SELECT room_id FROM bridge WHERE channel_id = ?")
                .bind(channel_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(result)
    }

    pub async fn list_channels(&self) -> crate::error::Result<Vec<String>> {
        let channels = sqlx::query_scalar::<_, String>("SELECT channel_id FROM bridge")
            .fetch_all(&self.pool)
            .await?;
        Ok(channels)
    }

    pub async fn list_all_bridges(&self) -> crate::error::Result<Vec<BridgedRoom>> {
        let bridges = sqlx::query_as::<_, BridgedRoom>("SELECT * FROM bridge")
            .fetch_all(&self.pool)
            .await?;
        Ok(bridges)
    }

    pub async fn remove_room(&self, room_id: &str) -> crate::error::Result<()> {
        sqlx::query("DELETE FROM bridge WHERE room_id = ?")
            .bind(room_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn add_user(&self, mxid: &str) -> crate::error::Result<()> {
        sqlx::query("INSERT OR IGNORE INTO users (mxid) VALUES (?)")
            .bind(mxid)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn fetch_user(&self, mxid: &str) -> crate::error::Result<Option<BridgedUser>> {
        let user = sqlx::query_as::<_, BridgedUser>("SELECT * FROM users WHERE mxid = ?")
            .bind(mxid)
            .fetch_optional(&self.pool)
            .await?;
        Ok(user)
    }

    pub async fn update_avatar(&self, mxid: &str, avatar_url: &str) -> crate::error::Result<()> {
        sqlx::query("UPDATE users SET avatar_url = ? WHERE mxid = ?")
            .bind(avatar_url)
            .bind(mxid)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn update_username(&self, mxid: &str, username: &str) -> crate::error::Result<()> {
        sqlx::query("UPDATE users SET username = ? WHERE mxid = ?")
            .bind(username)
            .bind(mxid)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn add_invite(
        &self,
        room_id: &str,
        sender: &str,
        room_name: Option<&str>,
    ) -> crate::error::Result<()> {
        sqlx::query("INSERT OR REPLACE INTO invites (room_id, sender, room_name) VALUES (?, ?, ?)")
            .bind(room_id)
            .bind(sender)
            .bind(room_name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_invites(&self) -> crate::error::Result<Vec<PendingInvite>> {
        let invites = sqlx::query_as::<_, PendingInvite>(
            "SELECT id, room_id, sender, room_name FROM invites ORDER BY id ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(invites)
    }

    pub async fn remove_invite(&self, id: i64) -> crate::error::Result<()> {
        sqlx::query("DELETE FROM invites WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn remove_invite_by_room(&self, room_id: &str) -> crate::error::Result<()> {
        sqlx::query("DELETE FROM invites WHERE room_id = ?")
            .bind(room_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn get_bridge(&self, room_id: &str) -> crate::error::Result<Option<BridgedRoom>> {
        let result = sqlx::query_as::<_, BridgedRoom>("SELECT * FROM bridge WHERE room_id = ?")
            .bind(room_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(result)
    }

    pub async fn update_bridge_config(
        &self,
        room_id: &str,
        setting: &str,
        value: bool,
    ) -> crate::error::Result<()> {
        let query = match setting {
            "d2m_enabled" => "UPDATE bridge SET d2m_enabled = ? WHERE room_id = ?",
            "m2d_enabled" => "UPDATE bridge SET m2d_enabled = ? WHERE room_id = ?",
            "d2m_mod_deletions" => "UPDATE bridge SET d2m_mod_deletions = ? WHERE room_id = ?",
            "m2d_mod_deletions" => "UPDATE bridge SET m2d_mod_deletions = ? WHERE room_id = ?",
            "d2m_typing" => "UPDATE bridge SET d2m_typing = ? WHERE room_id = ?",
            "m2d_typing" => "UPDATE bridge SET m2d_typing = ? WHERE room_id = ?",
            _ => {
                return Err(crate::error::BridgeError::Database(sqlx::Error::Protocol(
                    "Invalid setting".into(),
                )));
            }
        };

        sqlx::query(query)
            .bind(value)
            .bind(room_id)
            .execute(&self.pool)
            .await?;

        Ok(())
    }
}
