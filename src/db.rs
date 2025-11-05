use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Database {
    pool: SqlitePool,
}

#[derive(Debug, sqlx::FromRow)]
pub struct BridgedRoom {
    pub room_id: String,
    pub channel_id: String,
}

#[derive(Debug, sqlx::FromRow)]
pub struct BridgedUser {
    pub mxid: String,
    pub avatar_url: Option<String>,
    pub username: Option<String>,
}

impl Database {
    pub async fn new(path: impl AsRef<Path>) -> crate::error::Result<Self> {
        let url = format!("sqlite:{}", path.as_ref().display());
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await?;

        // Create tables
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS bridge (
                room_id TEXT PRIMARY KEY,
                channel_id TEXT NOT NULL
            )",
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

        Ok(Self { pool })
    }

    pub async fn add_room(&self, room_id: &str, channel_id: &str) -> crate::error::Result<()> {
        sqlx::query("INSERT INTO bridge (room_id, channel_id) VALUES (?, ?)")
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

    pub async fn list_channels(&self) -> crate::error::Result<Vec<String>> {
        let channels = sqlx::query_scalar::<_, String>("SELECT channel_id FROM bridge")
            .fetch_all(&self.pool)
            .await?;
        Ok(channels)
    }

    pub async fn add_user(&self, mxid: &str) -> crate::error::Result<()> {
        sqlx::query("INSERT INTO users (mxid) VALUES (?)")
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
}
