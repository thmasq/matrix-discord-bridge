use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Clone)]
pub struct Cache {
    // Matrix room alias -> room_id
    pub m_rooms: Arc<RwLock<HashMap<String, String>>>,
    // Matrix room_id -> member cache
    pub m_members: Arc<RwLock<HashMap<String, HashMap<String, MatrixUser>>>>,
    // Matrix event_id -> Discord message_id
    pub m_messages: Arc<RwLock<HashMap<String, String>>>,
    // Discord message_id -> Matrix event_id
    pub d_messages: Arc<RwLock<HashMap<String, String>>>,
    // Emote name -> Discord emote string
    pub d_emotes: Arc<RwLock<HashMap<String, String>>>,
    // Emote name -> Matrix MXC URL
    pub m_emotes: Arc<RwLock<HashMap<String, String>>>,
    // Discord channel_id -> webhook
    pub d_webhooks: Arc<RwLock<HashMap<String, WebhookInfo>>>,
    // Matrix room_id -> emoji shortcode -> MXC URL
    pub m_custom_emojis: Arc<RwLock<HashMap<String, HashMap<String, String>>>>,
    // Discord guild_id -> role_id -> role name
    pub d_roles: Arc<RwLock<HashMap<String, HashMap<String, String>>>>,
    // Discord guild_id -> channel_id -> channel name
    pub d_channels: Arc<RwLock<HashMap<String, HashMap<String, String>>>>,
}

#[derive(Debug, Clone)]
pub struct MatrixUser {
    pub avatar_url: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WebhookInfo {
    pub id: String,
    pub token: String,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            m_rooms: Arc::new(RwLock::new(HashMap::new())),
            m_members: Arc::new(RwLock::new(HashMap::new())),
            m_messages: Arc::new(RwLock::new(HashMap::new())),
            d_messages: Arc::new(RwLock::new(HashMap::new())),
            d_emotes: Arc::new(RwLock::new(HashMap::new())),
            m_emotes: Arc::new(RwLock::new(HashMap::new())),
            d_webhooks: Arc::new(RwLock::new(HashMap::new())),
            m_custom_emojis: Arc::new(RwLock::new(HashMap::new())),
            d_roles: Arc::new(RwLock::new(HashMap::new())),
            d_channels: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }
}
