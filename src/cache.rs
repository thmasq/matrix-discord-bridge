use moka::sync::Cache as MokaCache;
use std::collections::HashMap;
use std::time::Duration;

#[derive(Clone)]
pub struct Cache {
    // Matrix room alias -> room_id
    pub m_rooms: MokaCache<String, String>,

    // Matrix room_id -> member cache
    pub m_members: MokaCache<String, HashMap<String, MatrixUser>>,

    // Matrix event_id -> Discord message_id
    pub m_messages: MokaCache<String, String>,
    // Discord message_id -> Matrix event_id
    pub d_messages: MokaCache<String, String>,

    // Emote name -> Discord emote string
    pub d_emotes: MokaCache<String, String>,
    // Emote name -> Matrix MXC URL
    pub m_emotes: MokaCache<String, String>,

    // Discord channel_id -> webhook
    pub d_webhooks: MokaCache<String, WebhookInfo>,

    // Matrix room_id -> emoji shortcode -> MXC URL
    pub m_custom_emojis: MokaCache<String, HashMap<String, String>>,

    // Discord guild_id -> role_id -> role name
    pub d_roles: MokaCache<String, HashMap<String, String>>,
    // Discord guild_id -> channel_id -> channel name
    pub d_channels: MokaCache<String, HashMap<String, String>>,

    // Matrix MXC URL -> Cached avatar bytes
    pub m_avatars: MokaCache<String, Vec<u8>>,
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
            m_rooms: MokaCache::builder()
                .max_capacity(10_000)
                .time_to_idle(Duration::from_hours(168)) // 1 week
                .build(),

            m_members: MokaCache::builder()
                .max_capacity(1_000)
                .time_to_idle(Duration::from_hours(1)) // 1 hour idle (members change)
                .build(),

            m_messages: MokaCache::builder()
                .max_capacity(50_000)
                .time_to_idle(Duration::from_hours(168))
                .build(),

            d_messages: MokaCache::builder()
                .max_capacity(50_000)
                .time_to_idle(Duration::from_hours(168))
                .build(),

            d_emotes: MokaCache::builder()
                .max_capacity(10_000)
                .time_to_idle(Duration::from_hours(168))
                .build(),

            m_emotes: MokaCache::builder()
                .max_capacity(10_000)
                .time_to_idle(Duration::from_hours(168))
                .build(),

            d_webhooks: MokaCache::builder()
                .max_capacity(1_000)
                .time_to_idle(Duration::from_hours(24))
                .build(),

            m_custom_emojis: MokaCache::builder()
                .max_capacity(1_000)
                .time_to_idle(Duration::from_hours(24))
                .build(),

            d_roles: MokaCache::builder()
                .max_capacity(1_000)
                .time_to_idle(Duration::from_hours(24))
                .build(),

            d_channels: MokaCache::builder()
                .max_capacity(1_000)
                .time_to_idle(Duration::from_hours(24))
                .build(),

            m_avatars: MokaCache::builder()
                .weigher(|_key, value: &Vec<u8>| value.len().try_into().unwrap_or(u32::MAX))
                .max_capacity(50 * 1024 * 1024) // 50 MB max memory limit
                .time_to_idle(Duration::from_hours(24))
                .build(),
        }
    }
}

impl Cache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atomically insert bidirectional message mapping to prevent desyncs
    pub fn insert_message_mapping(&self, matrix_event_id: String, discord_message_id: String) {
        self.m_messages
            .insert(matrix_event_id.clone(), discord_message_id.clone());
        self.d_messages.insert(discord_message_id, matrix_event_id);
    }

    /// Remove bidirectional message mapping safely
    pub fn remove_message_mapping(
        &self,
        matrix_event_id: Option<&str>,
        discord_message_id: Option<&str>,
    ) {
        if let Some(m_id) = matrix_event_id {
            self.m_messages.invalidate(m_id);
        }
        if let Some(d_id) = discord_message_id {
            self.d_messages.invalidate(d_id);
        }
    }

    /// Clear guild data when bot is kicked/leaves
    pub fn remove_guild_data(&self, guild_id: &str) {
        self.d_roles.invalidate(guild_id);
        self.d_channels.invalidate(guild_id);
    }

    /// Clear room data when bot leaves/unbridges
    pub fn remove_room_data(&self, room_id: &str) {
        self.m_members.invalidate(room_id);
        self.m_custom_emojis.invalidate(room_id);

        let mut aliases_to_remove = Vec::new();
        for (alias, id) in &self.m_rooms {
            if id == room_id {
                aliases_to_remove.push(alias.clone());
            }
        }
        for alias in aliases_to_remove {
            self.m_rooms.invalidate(&*alias);
        }
    }
}
