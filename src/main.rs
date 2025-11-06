mod appservice;
mod cache;
mod config;
mod db;
mod discord_client;
mod error;
mod matrix_client;
mod utils;

use crate::{
    appservice::AppService, cache::Cache, config::Config, db::Database,
    discord_client::create_discord_client, matrix_client::MatrixClient,
};
use std::{env, path::PathBuf, sync::Arc};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Get base directory from args or use current directory
    let base_dir = env::args()
        .nth(1).map_or_else(|| env::current_dir().unwrap(), PathBuf::from);

    if !base_dir.exists() {
        anyhow::bail!("Path '{}' does not exist!", base_dir.display());
    }

    let config_path = base_dir.join("appservice.json");

    // Create default config if it doesn't exist
    if !config_path.exists() {
        Config::create_default(&config_path)?;
        tracing::info!("Created default config at {}", config_path.display());
        tracing::info!("Please edit the configuration file and restart.");
        return Ok(());
    }

    // Load configuration
    let mut config = Config::load(&config_path)?;

    // Make database path absolute if relative
    if config.database.is_relative() {
        config.database = base_dir.join(&config.database);
    }

    tracing::info!("Starting Matrix-Discord bridge");
    tracing::info!("Database: {}", config.database.display());
    tracing::info!("Homeserver: {}", config.homeserver);
    tracing::info!("Port: {}", config.port);

    // Initialize database
    let db = Database::new(&config.database).await?;
    tracing::info!("Database initialized");

    // Initialize cache
    let cache = Cache::new();

    // Create Matrix client
    let matrix = Arc::new(MatrixClient::new(config.clone(), db.clone(), cache.clone()));

    // Create appservice server
    let appservice = Arc::new(AppService::new(
        config.clone(),
        matrix.clone(),
        db.clone(),
        cache.clone(),
    ));

    // Start appservice in background
    let appservice_handle = {
        let appservice = appservice.clone();
        tokio::spawn(async move {
            if let Err(e) = appservice.run().await {
                tracing::error!("Appservice error: {}", e);
            }
        })
    };

    // Create and start Discord client
    tracing::info!("Connecting to Discord...");
    let mut discord = create_discord_client(
        config.discord_token.clone(),
        matrix.clone(),
        db.clone(),
        cache.clone(),
        config.clone(),
    )
    .await?;

    // Start Discord client
    let discord_handle = tokio::spawn(async move {
        if let Err(e) = discord.start().await {
            tracing::error!("Discord client error: {}", e);
        }
    });

    tracing::info!("Bridge is running!");

    // Wait for both tasks
    tokio::select! {
        _ = appservice_handle => {
            tracing::error!("Appservice stopped");
        }
        _ = discord_handle => {
            tracing::error!("Discord client stopped");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("Received shutdown signal");
        }
    }

    tracing::info!("Shutting down...");
    Ok(())
}
