use chrono::Utc;
use sqlx::{FromRow, SqlitePool};

#[derive(Debug, Clone, FromRow)]
pub struct Conversation {
    pub phone: String,
    pub display_name: Option<String>,
    pub latest_at: i64,
    pub message_count: i64,
    pub preview: String,
}

#[derive(Debug, Clone, FromRow)]
pub struct Message {
    pub direction: String,
    pub body: String,
    pub status: String,
    pub created_at: i64,
}

pub async fn initialize(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS contacts (
            phone TEXT PRIMARY KEY NOT NULL,
            display_name TEXT NOT NULL,
            source TEXT NOT NULL CHECK(source IN ('manual', 'whatsapp')),
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            wa_id TEXT NOT NULL UNIQUE,
            phone TEXT NOT NULL,
            direction TEXT NOT NULL CHECK(direction IN ('inbound', 'outbound')),
            body TEXT NOT NULL,
            status TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_messages_phone_created ON messages(phone, created_at)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_contact(
    pool: &SqlitePool,
    phone: &str,
    display_name: &str,
    source: &str,
) -> anyhow::Result<()> {
    let now = Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO contacts(phone, display_name, source, created_at, updated_at)
         VALUES(?, ?, ?, ?, ?)
         ON CONFLICT(phone) DO UPDATE SET
            display_name = excluded.display_name,
            source = excluded.source,
            updated_at = excluded.updated_at
         WHERE excluded.source = 'manual' OR contacts.source = 'whatsapp'",
    )
    .bind(phone)
    .bind(display_name)
    .bind(source)
    .bind(now)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn save_message(
    pool: &SqlitePool,
    wa_id: &str,
    phone: &str,
    direction: &str,
    body: &str,
    status: &str,
    created_at: Option<i64>,
) -> anyhow::Result<()> {
    let created_at = created_at.unwrap_or_else(|| Utc::now().timestamp());
    sqlx::query(
        "INSERT INTO messages(wa_id, phone, direction, body, status, created_at)
         VALUES(?, ?, ?, ?, ?, ?) ON CONFLICT(wa_id) DO NOTHING",
    )
    .bind(wa_id)
    .bind(phone)
    .bind(direction)
    .bind(body)
    .bind(status)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn update_status(pool: &SqlitePool, wa_id: &str, status: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE messages SET status = ? WHERE wa_id = ?")
        .bind(status)
        .bind(wa_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn conversations(
    pool: &SqlitePool,
    search: Option<&str>,
) -> anyhow::Result<Vec<Conversation>> {
    let pattern = search.map(|value| format!("%{}%", escape_like(value)));
    let rows = sqlx::query_as::<_, Conversation>(
        "SELECT messages.phone,
            (SELECT display_name FROM contacts WHERE contacts.phone = messages.phone) AS display_name,
            MAX(messages.created_at) AS latest_at, COUNT(*) AS message_count,
            (SELECT body FROM messages latest
             WHERE latest.phone = messages.phone
             ORDER BY latest.created_at DESC, latest.id DESC LIMIT 1) AS preview
         FROM messages
         WHERE ? IS NULL
            OR messages.phone LIKE ? ESCAPE '^'
            OR COALESCE((SELECT display_name FROM contacts WHERE contacts.phone = messages.phone), '') LIKE ? ESCAPE '^'
            OR EXISTS (
                SELECT 1 FROM messages searched
                WHERE searched.phone = messages.phone AND searched.body LIKE ? ESCAPE '^'
            )
         GROUP BY messages.phone ORDER BY latest_at DESC LIMIT 200",
    )
    .bind(pattern.as_deref())
    .bind(pattern.as_deref())
    .bind(pattern.as_deref())
    .bind(pattern.as_deref())
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

fn escape_like(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '^' | '%' | '_') {
            escaped.push('^');
        }
        escaped.push(character);
    }
    escaped
}

pub async fn messages_for(pool: &SqlitePool, phone: &str) -> anyhow::Result<Vec<Message>> {
    let rows = sqlx::query_as::<_, Message>(
        "SELECT direction, body, status, created_at
         FROM (
            SELECT id, wa_id, phone, direction, body, status, created_at
            FROM messages WHERE phone = ? ORDER BY created_at DESC, id DESC LIMIT 300
         ) ORDER BY created_at ASC, id ASC",
    )
    .bind(phone)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}
