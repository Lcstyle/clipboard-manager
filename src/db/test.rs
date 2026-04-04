use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use serial_test::serial;

use anyhow::Result;
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use crate::{
    config::Config,
    db::{DbSqlite, DbTrait, DbPersistOp, EntryTrait},
};

use super::{Content, EntryId, MimeDataMap, MimeType, TimestampMillis, find_alt};

// ── MimeType ────────────────────────────────────────────────────────

#[test]
fn test_mime_type_display() {
    let m = MimeType::new("text/plain".into());
    assert_eq!(m.to_string(), "text/plain");
    assert_eq!(m.as_str(), "text/plain");
}

#[test]
fn test_mime_type_eq_hash() {
    let mut map = HashMap::new();
    map.insert(MimeType::new("text/plain".into()), vec![1u8]);
    assert!(map.contains_key(&MimeType::new("text/plain".into())));
    assert!(!map.contains_key(&MimeType::new("image/png".into())));
}

#[test]
fn test_mime_type_clone() {
    let a = MimeType::new("image/png".into());
    let b = a.clone();
    assert_eq!(a, b);
}

// ── EntryId ─────────────────────────────────────────────────────────

#[test]
fn test_entry_id_display() {
    let id = EntryId(12345);
    assert_eq!(id.to_string(), "12345");
    assert_eq!(id.as_i64(), 12345);
}

#[test]
fn test_entry_id_eq_copy() {
    let a = EntryId(1);
    let b = a; // Copy
    assert_eq!(a, b);
}

#[test]
fn test_entry_id_ord() {
    assert!(EntryId(1) < EntryId(2));
    assert_eq!(EntryId(5), EntryId(5));
}

#[test]
fn test_entry_id_hash() {
    let mut map = HashMap::new();
    map.insert(EntryId(42), "hello");
    assert_eq!(map.get(&EntryId(42)), Some(&"hello"));
    assert_eq!(map.get(&EntryId(99)), None);
}

// ── TimestampMillis ─────────────────────────────────────────────────

#[test]
fn test_timestamp_millis_display() {
    let t = TimestampMillis(1000);
    assert_eq!(t.as_i64(), 1000);
    assert_eq!(t.to_string(), "1000");
}

#[test]
fn test_timestamp_millis_ord() {
    assert!(TimestampMillis(100) < TimestampMillis(200));
    assert!(TimestampMillis(300) > TimestampMillis(200));
    assert_eq!(TimestampMillis(50), TimestampMillis(50));
}

#[test]
fn test_timestamp_millis_copy() {
    let a = TimestampMillis(999);
    let b = a; // Copy
    assert_eq!(a, b);
}

#[test]
fn test_timestamp_millis_hash() {
    let mut map = HashMap::new();
    map.insert(TimestampMillis(123), "ts");
    assert!(map.contains_key(&TimestampMillis(123)));
    assert!(!map.contains_key(&TimestampMillis(456)));
}

// ── Content ─────────────────────────────────────────────────────────

#[test]
fn test_content_text() {
    let raw = b"hello world";
    let c = Content::try_new("text/plain", raw).unwrap();
    assert!(matches!(c, Some(Content::Text("hello world"))));
}

#[test]
fn test_content_image() {
    let raw: &[u8] = &[0xFF, 0xD8, 0xFF];
    let c = Content::try_new("image/png", raw).unwrap();
    assert!(matches!(c, Some(Content::Image(_))));
}

#[test]
fn test_content_uri_list() {
    let raw = b"file:///a\nfile:///b\n# comment\n";
    let c = Content::try_new("text/uri-list", raw).unwrap();
    match c {
        Some(Content::UriList(uris)) => {
            assert_eq!(uris, vec!["file:///a", "file:///b"]);
        }
        other => panic!("expected UriList, got {other:?}"),
    }
}

#[test]
fn test_content_unsupported_mime() {
    let raw = b"data";
    let c = Content::try_new("application/octet-stream", raw).unwrap();
    assert!(c.is_none());
}

// ── find_alt ────────────────────────────────────────────────────────

#[test]
fn test_find_alt_present() {
    let html = r#"<img src="x.png" alt="my image" />"#;
    assert_eq!(find_alt(html), Some("my image"));
}

#[test]
fn test_find_alt_missing() {
    let html = "<p>no image here</p>";
    assert_eq!(find_alt(html), None);
}

// ── DbPersistOp ─────────────────────────────────────────────────────

#[test]
fn test_db_persist_op_clone_debug() {
    let op = DbPersistOp::Delete { id: EntryId(1) };
    let op2 = op.clone();
    let _ = format!("{op2:?}");
}

fn prepare_db_dir() -> PathBuf {
    let fmt_layer = fmt::layer().with_target(false);
    let filter_layer = EnvFilter::try_from_default_env().unwrap_or(EnvFilter::new(format!(
        "warn,{}=info",
        env!("CARGO_CRATE_NAME")
    )));
    let _ = tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .try_init();

    let db_dir = PathBuf::from("tests");
    let _ = std::fs::create_dir_all(&db_dir);
    remove_dir_contents(&db_dir);
    db_dir
}

#[tokio::test]
#[serial]
async fn test() -> Result<()> {
    let db_dir = prepare_db_dir();

    let mut db = DbSqlite::with_path(&Config::default(), &db_dir).await?;

    test_db(&mut db).await.unwrap();

    db.clear().await?;

    test_db(&mut db).await.unwrap();

    Ok(())
}

fn build_content(content: &[(&str, &str)]) -> MimeDataMap {
    content
        .iter()
        .map(|(mime, content)| (MimeType::new(mime.to_string()), content.as_bytes().into()))
        .collect()
}

async fn test_db(db: &mut DbSqlite) -> Result<()> {
    assert!(db.len() == 0);

    let data = build_content(&[("text/plain", "content")]);

    db.insert_with_time(data.clone(), TimestampMillis(10)).await.unwrap();

    assert!(db.len() == 1);

    tokio::time::sleep(Duration::from_millis(1000)).await;

    db.insert_with_time(data.clone(), TimestampMillis(20)).await.unwrap();

    assert!(db.len() == 1);

    tokio::time::sleep(Duration::from_millis(1000)).await;

    let data2 = build_content(&[("text/plain", "content2")]);

    db.insert_with_time(data2.clone(), TimestampMillis(30)).await.unwrap();

    assert_eq!(db.len(), 2);

    let next = db.iter().next().unwrap();

    assert!(next.raw_content == data2);

    Ok(())
}

#[tokio::test]
#[serial]
async fn test_delete_old_one() {
    let db_path = prepare_db_dir();

    let mut db = DbSqlite::with_path(&Config::default(), &db_path)
        .await
        .unwrap();

    let data = build_content(&[("text/plain", "content")]);

    db.insert(data).await.unwrap();

    tokio::time::sleep(Duration::from_millis(100)).await;

    let data = build_content(&[("text/plain", "content2")]);

    db.insert(data).await.unwrap();

    assert_eq!(db.len(), 2);

    drop(db);
    let db = DbSqlite::with_path(&Config::default(), &db_path)
        .await
        .unwrap();

    assert_eq!(db.len(), 2);

    let config = Config {
        maximum_entries_lifetime: Some(0),
        ..Default::default()
    };
    drop(db);
    let db = DbSqlite::with_path(&config, &db_path).await.unwrap();

    assert_eq!(db.len(), 0);
}

#[tokio::test]
#[serial]
async fn same() {
    let db_path = prepare_db_dir();

    let mut db = DbSqlite::with_path(&Config::default(), &db_path)
        .await
        .unwrap();

    let data = build_content(&[("text/plain", "content")]);

    db.insert(data.clone()).await.unwrap();
    db.insert(data.clone()).await.unwrap();
    assert!(db.len() == 1);
}

#[tokio::test]
#[serial]
async fn favorites() {
    let db_path = prepare_db_dir();

    let mut db = DbSqlite::with_path(&Config::default(), &db_path)
        .await
        .unwrap();

    let now1 = 1000i64;
    let id1 = EntryId(now1);
    let data1 = build_content(&[("text/plain", "content1")]);
    db.insert_with_time(data1, TimestampMillis(now1)).await.unwrap();

    let now2 = 2000i64;
    let id2 = EntryId(now2);
    let data2 = build_content(&[("text/plain", "content2")]);
    db.insert_with_time(data2, TimestampMillis(now2)).await.unwrap();

    let now3 = 3000i64;
    let id3 = EntryId(now3);
    let data3 = build_content(&[("text/plain", "content3")]);
    db.insert_with_time(data3.clone(), TimestampMillis(now3)).await.unwrap();

    db.add_favorite(id3, None).await.unwrap();

    assert!(db.get_from_id(id3).unwrap().is_favorite);
    assert_eq!(db.favorites.len(), 1);

    db.delete(id3).await.unwrap();

    assert_eq!(db.favorites.len(), 0);

    db.insert_with_time(data3.clone(), TimestampMillis(now3)).await.unwrap();

    db.add_favorite(id1, None).await.unwrap();

    db.add_favorite(id3, None).await.unwrap();

    db.add_favorite(id2, Some(1)).await.unwrap();

    assert_eq!(db.favorites.len(), 3);

    assert_eq!(db.favorites.fav(), &vec![id1, id2, id3]);

    db.remove_favorite(id2).await.unwrap();

    assert_eq!(db.len(), 3);

    drop(db);
    let db = DbSqlite::with_path(
        &Config {
            maximum_entries_lifetime: Some(0),
            ..Default::default()
        },
        &db_path,
    )
    .await
    .unwrap();

    assert_eq!(db.len(), 2);

    assert_eq!(db.favorites.len(), 2);
    assert_eq!(db.favorites.fav(), &vec![id1, id3]);
}

#[tokio::test]
#[serial]
async fn lock() {
    let db_path = prepare_db_dir();

    let mut db1 = DbSqlite::with_path(&Config::default(), &db_path)
        .await
        .unwrap();

    let mut db2 = DbSqlite::with_path(&Config::default(), &db_path)
        .await
        .unwrap();

    let now1 = 1000;
    let data1 = build_content(&[("text/plain", "content1")]);
    db1.insert_with_time(data1, TimestampMillis(now1)).await.unwrap();

    let now2 = 2000;
    let data2 = build_content(&[("text/plain", "content2")]);
    db1.insert_with_time(data2, TimestampMillis(now2)).await.unwrap();

    let now3 = 3000;
    let data3 = build_content(&[("text/plain", "content3")]);
    db1.insert_with_time(data3.clone(), TimestampMillis(now3)).await.unwrap();

    let now4 = 4000;
    let data4 = build_content(&[("text/plain", "content3")]);
    db2.insert_with_time(data4.clone(), TimestampMillis(now4)).await.unwrap();

    assert_eq!(db1.len(), 3);
    assert_eq!(db2.len(), 0);
}

fn remove_dir_contents(dir: &Path) {
    pub fn inner(dir: &Path) -> Result<(), std::io::Error> {
        for entry in fs::read_dir(dir)?.flatten() {
            let path = entry.path();

            if path.is_dir() {
                let _ = fs::remove_dir_all(&path);
            } else {
                let _ = fs::remove_file(&path);
            }
        }
        Ok(())
    }

    let _ = inner(dir);
}

use std::time::Instant;

#[tokio::test]
#[ignore = "bench"]
async fn bench_search_from_system_path() {
    let mut db = DbSqlite::new(&Config::default()).await.unwrap();

    let now = Instant::now();

    println!("{}", db.len());

    db.set_query_and_search("a".into());

    println!("Elapsed: {:?}", now.elapsed());
}

// ── insert_to_memory / delete_from_memory / clear_memory ────────────

#[tokio::test]
#[serial]
async fn test_insert_to_memory_returns_persist_op() {
    let db_dir = prepare_db_dir();
    let mut db = DbSqlite::with_path(&Config::default(), &db_dir)
        .await
        .unwrap();
    let data = build_content(&[("text/plain", "memory test")]);
    let op = db.insert_to_memory(data);
    assert!(op.is_some(), "insert_to_memory should return Some(InsertNew)");
    assert!(matches!(op.unwrap(), DbPersistOp::InsertNew { .. }));
}

#[tokio::test]
#[serial]
async fn test_insert_to_memory_dedup_returns_update_timestamp() {
    let db_dir = prepare_db_dir();
    let mut db = DbSqlite::with_path(&Config::default(), &db_dir)
        .await
        .unwrap();
    let data = build_content(&[("text/plain", "dup test")]);
    let _ = db.insert_to_memory(data.clone());
    let op = db.insert_to_memory(data);
    assert!(op.is_some());
    assert!(matches!(op.unwrap(), DbPersistOp::UpdateTimestamp { .. }));
}

#[tokio::test]
#[serial]
async fn test_delete_from_memory_returns_persist_op() {
    let db_dir = prepare_db_dir();
    let mut db = DbSqlite::with_path(&Config::default(), &db_dir)
        .await
        .unwrap();
    let data = build_content(&[("text/plain", "to delete")]);
    db.insert(data).await.unwrap();
    let first_id = db.iter().next().expect("should have an entry").id();
    let op = db.delete_from_memory(first_id);
    assert!(op.is_some());
    assert!(matches!(op.unwrap(), DbPersistOp::Delete { .. }));
}

#[tokio::test]
#[serial]
async fn test_delete_from_memory_missing_returns_none() {
    let db_dir = prepare_db_dir();
    let mut db = DbSqlite::with_path(&Config::default(), &db_dir)
        .await
        .unwrap();
    let op = db.delete_from_memory(EntryId(99999));
    assert!(op.is_none());
}

#[tokio::test]
#[serial]
async fn test_clear_memory_returns_persist_op() {
    let db_dir = prepare_db_dir();
    let mut db = DbSqlite::with_path(&Config::default(), &db_dir)
        .await
        .unwrap();
    db.insert(build_content(&[("text/plain", "a")])).await.unwrap();
    db.insert(build_content(&[("text/plain", "b")])).await.unwrap();
    let op = db.clear_memory();
    assert!(op.is_some());
    assert!(matches!(op.unwrap(), DbPersistOp::ClearNonFavorites));
    assert_eq!(db.len(), 0);
}
