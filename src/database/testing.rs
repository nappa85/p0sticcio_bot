//! helpers for tests running against a real database, see `docker/test-db.sh`

use ingress_intel_rs::entities::IntelPortal;
use sea_orm::{ConnectionTrait, Database, DatabaseConnection};
use sea_orm_migration::MigratorTrait;
use tokio::sync::{Mutex, MutexGuard};

/// tests share the database, run them one at a time
static LOCK: Mutex<()> = Mutex::const_new(());

pub struct TestDb {
    pub conn: DatabaseConnection,
    _guard: MutexGuard<'static, ()>,
}

/// connects to `TEST_DATABASE_URL`, migrates it and empties every table, None (test skipped) when it isn't set
pub async fn db() -> Option<TestDb> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("TEST_DATABASE_URL not set, skipping database test");
        return None;
    };
    let guard = LOCK.lock().await;
    let conn = Database::connect(url).await.expect("test database connection failed");
    super::migrations::Migrator::up(&conn, None).await.expect("test database migration failed");
    conn.execute_unprepared(
        "truncate portals, portal_revisions, portal_mods, portal_resonators, locations, portal_watches restart identity",
    )
    .await
    .expect("test database truncate failed");
    Some(TestDb { conn, _guard: guard })
}

pub struct Spec<'a> {
    pub title: &'a str,
    pub faction: &'a str,
    pub lat: f64,
    pub lon: f64,
    pub level: u8,
    pub health: u8,
    pub owner: Option<&'a str>,
    /// (owner, level, energy)
    pub resonators: &'a [(&'a str, u8, u16)],
    /// (owner, name, rarity)
    pub mods: &'a [(&'a str, &'a str, &'a str)],
}

impl Default for Spec<'_> {
    fn default() -> Self {
        Spec {
            title: "Portal",
            faction: "N",
            lat: 45.0,
            lon: 11.0,
            level: 1,
            health: 0,
            owner: None,
            resonators: &[],
            mods: &[],
        }
    }
}

/// builds a portal as intel would return it
pub fn intel_portal(spec: Spec<'_>) -> IntelPortal {
    let mut mods =
        spec.mods.iter().map(|(owner, name, rarity)| serde_json::json!([owner, name, rarity, {}])).collect::<Vec<_>>();
    mods.resize(4, serde_json::Value::Null);
    serde_json::from_value(serde_json::json!({
        "tag": "p",
        "faction": spec.faction,
        "latitude": (spec.lat * 1e6).round() as i64,
        "longitude": (spec.lon * 1e6).round() as i64,
        "level": spec.level,
        "health": spec.health,
        "res_count": spec.resonators.len(),
        "image": "https://example.com/image.jpg",
        "title": spec.title,
        "ornaments": [],
        "mission": false,
        "mission_50plus": false,
        "artifact_brief": null,
        "timestamp": 0,
        "mods": if spec.mods.is_empty() { serde_json::Value::Null } else { serde_json::Value::from(mods) },
        "resonators": spec.resonators.iter().map(|(owner, level, energy)| serde_json::json!([owner, level, energy])).collect::<Vec<_>>(),
        "owner": spec.owner,
    }))
    .expect("invalid intel portal")
}

/// stores a new revision of the portal, like a scan would
pub async fn scan(conn: &DatabaseConnection, portal_id: &str, timestamp: i64, spec: Spec<'_>) {
    let old = super::portal::Entity::get_latest_revision(conn, portal_id).await.unwrap();
    super::portal::update_or_insert(conn, portal_id, timestamp, old, intel_portal(spec)).await.unwrap();
}
