use std::{collections::HashMap, fmt::Debug, path::Path, sync::LazyLock};

use anyhow::Result;

use chrono::Utc;
use regex::Regex;

use crate::config::Config;

#[cfg(test)]
pub mod test;

mod sqlite_db;
pub use sqlite_db::DbSqlite;

fn now() -> TimestampMillis {
    TimestampMillis(Utc::now().timestamp_millis())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId(pub(crate) i64);

impl EntryId {
    pub fn as_i64(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for EntryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Sqlite> for EntryId {
    fn decode(
        value: <sqlx::Sqlite as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        let inner = <i64 as sqlx::Decode<sqlx::Sqlite>>::decode(value)?;
        Ok(EntryId(inner))
    }
}

impl sqlx::Type<sqlx::Sqlite> for EntryId {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <i64 as sqlx::Type<sqlx::Sqlite>>::type_info()
    }
}

impl<'q> sqlx::Encode<'q, sqlx::Sqlite> for EntryId {
    fn encode_by_ref(
        &self,
        buf: &mut <sqlx::Sqlite as sqlx::Database>::ArgumentBuffer<'q>,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        <i64 as sqlx::Encode<sqlx::Sqlite>>::encode_by_ref(&self.0, buf)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MimeType(pub(crate) String);

impl MimeType {
    pub fn new(s: String) -> Self {
        Self(s)
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MimeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Milliseconds since Unix epoch. Used for entry creation timestamps.
/// Distinct from EntryId (which happens to also be a millisecond timestamp)
/// to prevent accidental mixing of IDs and times.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimestampMillis(pub(crate) i64);

impl TimestampMillis {
    pub fn as_i64(self) -> i64 {
        self.0
    }
}

impl std::fmt::Display for TimestampMillis {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl sqlx::Type<sqlx::Sqlite> for TimestampMillis {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <i64 as sqlx::Type<sqlx::Sqlite>>::type_info()
    }
}

impl<'q> sqlx::Encode<'q, sqlx::Sqlite> for TimestampMillis {
    fn encode_by_ref(
        &self,
        buf: &mut <sqlx::Sqlite as sqlx::Database>::ArgumentBuffer<'q>,
    ) -> Result<sqlx::encode::IsNull, sqlx::error::BoxDynError> {
        <i64 as sqlx::Encode<sqlx::Sqlite>>::encode_by_ref(&self.0, buf)
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Sqlite> for TimestampMillis {
    fn decode(
        value: <sqlx::Sqlite as sqlx::Database>::ValueRef<'r>,
    ) -> Result<Self, sqlx::error::BoxDynError> {
        <i64 as sqlx::Decode<sqlx::Sqlite>>::decode(value).map(TimestampMillis)
    }
}

pub type RawContent = Vec<u8>;
pub type MimeDataMap = HashMap<MimeType, RawContent>;

pub enum Content<'a> {
    Text(&'a str),
    Image(&'a [u8]),
    UriList(Vec<&'a str>),
}

impl<'a> Content<'a> {
    fn try_new(mime: &str, content: &'a [u8]) -> Result<Option<Self>> {
        if mime == "text/uri-list" {
            let text = core::str::from_utf8(content)?;

            let uris = text
                .lines()
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .collect();

            return Ok(Some(Content::UriList(uris)));
        }

        if mime.starts_with("text/") {
            return Ok(Some(Content::Text(core::str::from_utf8(content)?)));
        }

        if mime.starts_with("image/") {
            return Ok(Some(Content::Image(content)));
        }

        Ok(None)
    }
}

impl Debug for Content<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(arg0) => f.debug_tuple("Text").field(arg0).finish(),
            Self::UriList(arg0) => f.debug_tuple("UriList").field(arg0).finish(),
            Self::Image(_) => f.debug_tuple("Image").finish(),
        }
    }
}

/// More we have mime types here, Less we spend time in the [`EntryTrait::preferred_content`] function.
const PRIV_MIME_TYPES_SIMPLE: &[&str] = &[
    "image/png",
    "image/jpg",
    "image/jpeg",
    "image/bmp",
    "text/plain;charset=utf-8",
    "text/plain",
    "STRING",
    "UTF8_STRING",
    "TEXT",
];
const PRIV_MIME_TYPES_REGEX_STR: &[&str] = &["text/plain*", "text/*", "image/*"];

static PRIV_MIME_TYPES_REGEX: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    PRIV_MIME_TYPES_REGEX_STR
        .iter()
        .map(|r| Regex::new(r).unwrap())
        .collect()
});

pub trait EntryTrait: Debug + Clone + Send {
    fn is_favorite(&self) -> bool;

    fn favorite_title(&self) -> Option<&str>;

    fn raw_content(&self) -> &MimeDataMap;

    #[allow(dead_code)]
    fn into_raw_content(self) -> MimeDataMap;

    fn id(&self) -> EntryId;

    // note: hot fn, do not log
    fn preferred_content(
        &self,
        preferred_mime_types: &[Regex],
    ) -> Option<((&str, &RawContent), Content<'_>)> {
        for pref_mime_regex in preferred_mime_types {
            for (mime, raw_content) in self.raw_content() {
                if !raw_content.is_empty() && pref_mime_regex.is_match(mime.as_str()) {
                    match Content::try_new(mime.as_str(), raw_content) {
                        Ok(Some(content)) => return Some(((mime.as_str(), raw_content), content)),
                        Ok(None) => {
                            // unsupported mime type
                        }
                        Err(_e) => {
                            tracing::debug!("failed to parse content: {_e}");
                        }
                    }
                }
            }
        }

        for pref_mime in PRIV_MIME_TYPES_SIMPLE {
            let key = MimeType::new(pref_mime.to_string());
            if let Some(raw_content) = self.raw_content().get(&key)
                && !raw_content.is_empty()
            {
                match Content::try_new(pref_mime, raw_content) {
                    Ok(Some(content)) => return Some(((pref_mime, raw_content), content)),
                    Ok(None) => {}
                    Err(_e) => {
                        tracing::debug!("failed to parse content: {_e}");
                    }
                }
            }
        }

        for pref_mime_regex in PRIV_MIME_TYPES_REGEX.iter() {
            for (mime, raw_content) in self.raw_content() {
                if !raw_content.is_empty() && pref_mime_regex.is_match(mime.as_str()) {
                    match Content::try_new(mime.as_str(), raw_content) {
                        Ok(Some(content)) => return Some(((mime.as_str(), raw_content), content)),
                        Ok(None) => {}
                        Err(_e) => {
                            tracing::debug!("failed to parse content: {_e}");
                        }
                    }
                }
            }
        }

        None
    }

    fn searchable_content(&self) -> impl Iterator<Item = &str> {
        self.raw_content().iter().filter_map(|(mime, content)| {
            if mime.as_str().starts_with("text/") {
                let text = core::str::from_utf8(content).ok()?;

                if mime.as_str() == "text/html"
                    && let Some(alt) = find_alt(text)
                {
                    return Some(alt);
                }

                return Some(text);
            }

            None
        })
    }
}

pub trait DbTrait: Sized {
    type Entry: EntryTrait;

    async fn new(config: &Config) -> Result<Self>;

    async fn with_path(config: &Config, db_dir: &Path) -> Result<Self>;

    async fn reload(&mut self) -> Result<()>;

    async fn insert(&mut self, data: MimeDataMap) -> Result<()>;

    async fn insert_with_time(&mut self, data: MimeDataMap, time: TimestampMillis) -> Result<()>;

    /// Update in-memory state for an insert (dedup, eviction) without touching
    /// SQLite.  Returns `None` when the DB lock is not held (no-op) or a
    /// [`DbPersistOp`] that must be executed on a background connection to keep
    /// the on-disk state in sync.
    fn insert_to_memory(&mut self, data: MimeDataMap) -> Option<DbPersistOp>;

    /// Delete an entry from in-memory state only, returning the persist op.
    fn delete_from_memory(&mut self, id: EntryId) -> Option<DbPersistOp>;

    /// Clear non-favorite entries from in-memory state only, returning the persist op.
    fn clear_memory(&mut self) -> Option<DbPersistOp>;

    /// Path to the SQLite database file (for opening background connections).
    fn db_path(&self) -> &str;

    /// Add a favorite in-memory only, returning the persist op for background SQL.
    fn add_favorite_to_memory(&mut self, id: EntryId, index: Option<usize>) -> DbPersistOp;

    /// Remove a favorite in-memory only, returning the persist op for background SQL.
    fn remove_favorite_from_memory(&mut self, id: EntryId) -> DbPersistOp;

    /// Set a favorite's title in-memory only, returning the persist op for background SQL.
    fn set_favorite_title_in_memory(&mut self, id: EntryId, title: Option<String>) -> DbPersistOp;

    /// Update an entry's content in-memory only, returning the persist op for background SQL.
    /// Returns `None` if dedup detected (the caller should handle that case separately).
    fn update_content_in_memory(&mut self, id: EntryId, data: MimeDataMap) -> Option<DbPersistOp>;

    #[must_use = "returns the (possibly different) EntryId after dedup"]
    async fn update_content(&mut self, id: EntryId, data: MimeDataMap) -> Result<EntryId>;

    async fn delete(&mut self, data: EntryId) -> Result<()>;

    async fn clear(&mut self) -> Result<()>;

    async fn add_favorite(&mut self, entry: EntryId, index: Option<usize>) -> Result<()>;

    async fn remove_favorite(&mut self, entry: EntryId) -> Result<()>;

    async fn set_favorite_title(&mut self, id: EntryId, title: Option<String>) -> Result<()>;

    fn search(&mut self);

    fn set_query_and_search(&mut self, query: String);

    fn get_query(&self) -> &str;

    fn get(&self, index: usize) -> Option<&Self::Entry>;

    fn get_from_id(&self, id: EntryId) -> Option<&Self::Entry>;

    fn iter(&self) -> impl Iterator<Item = &'_ Self::Entry>;

    fn chronological_iter(&self) -> impl Iterator<Item = &'_ Self::Entry>;

    fn search_iter(&self) -> impl Iterator<Item = &'_ Self::Entry>;

    fn either_iter(
        &self,
    ) -> itertools::Either<
        impl Iterator<Item = &'_ Self::Entry>,
        impl Iterator<Item = &'_ Self::Entry>,
    >;

    fn len(&self) -> usize;

    async fn handle_message(&mut self, message: DbMessage) -> Result<()>;

    fn is_search_active(&self) -> bool {
        !self.get_query().is_empty()
    }
}

/// Represents a SQL operation to be persisted in the background.
/// The in-memory state is updated synchronously; these ops capture
/// the corresponding SQL writes to be executed on a separate connection.
#[derive(Clone, Debug)]
pub enum DbPersistOp {
    /// Duplicate entry detected — just bump its timestamp.
    UpdateTimestamp {
        id: EntryId,
        new_time: TimestampMillis,
    },
    /// Brand new entry — insert rows and optionally evict old entries.
    InsertNew {
        id: EntryId,
        time: TimestampMillis,
        data: MimeDataMap,
        evictions: Vec<EntryId>,
    },
    /// Delete an entry.
    Delete {
        id: EntryId,
    },
    /// Clear all non-favorite entries.
    ClearNonFavorites,
    /// Add an entry to favorites.
    AddFavorite {
        id: EntryId,
        position: usize,
        bump_from: Option<usize>,
    },
    /// Remove an entry from favorites, adjusting positions.
    RemoveFavorite {
        id: EntryId,
        old_position: Option<usize>,
    },
    /// Set or clear a favorite's title.
    SetFavoriteTitle {
        id: EntryId,
        title: Option<String>,
    },
    /// Update an entry's content (delete old rows, insert new ones, bump timestamp).
    UpdateContent {
        id: EntryId,
        data: MimeDataMap,
        new_time: TimestampMillis,
    },
}

#[derive(Clone, Debug)]
pub enum DbMessage {
    CheckUpdate,
}
/// Execute a [`DbPersistOp`] on a freshly opened SQLite connection.
/// This runs on a background thread so the UI stays responsive.
pub async fn persist_op(db_path: &str, op: DbPersistOp) -> Result<()> {
    use sqlx::SqliteConnection;
    use sqlx::prelude::*;

    let mut conn = SqliteConnection::connect(db_path).await?;

    match op {
        DbPersistOp::UpdateTimestamp { id, new_time } => {
            sqlx::query("UPDATE ClipboardEntries SET creation = $1 WHERE id = $2")
                .bind(new_time)
                .bind(id)
                .execute(&mut conn)
                .await?;
        }
        DbPersistOp::InsertNew {
            id,
            time,
            data,
            evictions,
        } => {
            sqlx::query("INSERT INTO ClipboardEntries (id, creation) SELECT $1, $2")
                .bind(id)
                .bind(time)
                .execute(&mut conn)
                .await?;

            for (mime, content) in &data {
                sqlx::query(
                    "INSERT INTO ClipboardContents (id, mime, content) SELECT $1, $2, $3",
                )
                .bind(id)
                .bind(mime.as_str())
                .bind(content)
                .execute(&mut conn)
                .await?;
            }

            for evict_id in evictions {
                sqlx::query("DELETE FROM ClipboardEntries WHERE id = ?")
                    .bind(evict_id)
                    .execute(&mut conn)
                    .await?;
            }
        }
        DbPersistOp::Delete { id } => {
            sqlx::query("DELETE FROM ClipboardEntries WHERE id = ?")
                .bind(id)
                .execute(&mut conn)
                .await?;
        }
        DbPersistOp::ClearNonFavorites => {
            sqlx::query(
                "DELETE FROM ClipboardEntries WHERE id NOT IN (SELECT id FROM FavoriteClipboardEntries)",
            )
            .execute(&mut conn)
            .await?;
        }
        DbPersistOp::AddFavorite {
            id,
            position,
            bump_from,
        } => {
            if let Some(bump_pos) = bump_from {
                sqlx::query(
                    "UPDATE FavoriteClipboardEntries SET position = position + 1 WHERE position >= ?",
                )
                .bind(bump_pos as i32)
                .execute(&mut conn)
                .await?;
            }
            sqlx::query(
                "INSERT INTO FavoriteClipboardEntries (id, position, title) VALUES ($1, $2, $3)",
            )
            .bind(id)
            .bind(position as i32)
            .bind(Option::<String>::None)
            .execute(&mut conn)
            .await?;
        }
        DbPersistOp::RemoveFavorite { id, old_position } => {
            sqlx::query("DELETE FROM FavoriteClipboardEntries WHERE id = ?")
                .bind(id)
                .execute(&mut conn)
                .await?;
            if let Some(pos) = old_position {
                sqlx::query(
                    "UPDATE FavoriteClipboardEntries SET position = position - 1 WHERE position >= ?",
                )
                .bind(pos as i32)
                .execute(&mut conn)
                .await?;
            }
        }
        DbPersistOp::SetFavoriteTitle { id, title } => {
            sqlx::query("UPDATE FavoriteClipboardEntries SET title = ? WHERE id = ?")
                .bind(&title)
                .bind(id)
                .execute(&mut conn)
                .await?;
        }
        DbPersistOp::UpdateContent { id, data, new_time } => {
            sqlx::query("DELETE FROM ClipboardContents WHERE id = ?")
                .bind(id)
                .execute(&mut conn)
                .await?;
            for (mime, content) in &data {
                sqlx::query(
                    "INSERT INTO ClipboardContents (id, mime, content) SELECT $1, $2, $3",
                )
                .bind(id)
                .bind(mime.as_str())
                .bind(content)
                .execute(&mut conn)
                .await?;
            }
            sqlx::query("UPDATE ClipboardEntries SET creation = $1 WHERE id = $2")
                .bind(new_time)
                .bind(id)
                .execute(&mut conn)
                .await?;
        }
    }

    Ok(())
}

// currently best effort
fn find_alt(html: &str) -> Option<&str> {
    let alt = html.split_once("alt=\"")?.1.split_once('"')?.0;
    Some(alt)
}
