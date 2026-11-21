//! schema migrations, applied when the application starts

use sea_orm_migration::{
    MigrationName, MigrationTrait, MigratorTrait,
    async_trait::async_trait,
    prelude::{DbErr, SchemaManager},
    sea_orm::ConnectionTrait,
};

pub struct Migrator;

#[async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(m20261121_000001_baseline::Migration), Box::new(m20261121_000002_portal_watches::Migration)]
    }
}

mod m20261121_000001_baseline {
    use super::*;

    pub struct Migration;

    // the derive would name it after the file
    impl MigrationName for Migration {
        fn name(&self) -> &str {
            "m20261121_000001_baseline"
        }
    }

    #[async_trait]
    impl MigrationTrait for Migration {
        async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
            manager.get_connection().execute_unprepared(include_str!("baseline.sql")).await.map(drop)
        }
    }
}

mod m20261121_000002_portal_watches {
    use super::*;

    pub struct Migration;

    impl MigrationName for Migration {
        fn name(&self) -> &str {
            "m20261121_000002_portal_watches"
        }
    }

    #[async_trait]
    impl MigrationTrait for Migration {
        async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
            manager.get_connection().execute_unprepared(include_str!("portal_watches.sql")).await.map(drop)
        }

        async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
            manager.get_connection().execute_unprepared("DROP TABLE portal_watches").await.map(drop)
        }
    }
}

#[cfg(test)]
mod tests {
    use sea_orm_migration::sea_orm::{Database, DatabaseConnection, FromQueryResult, Statement};

    use super::*;

    /// the schema.sql used to create databases before migrations
    const LEGACY: &str = include_str!("legacy_schema.sql");

    #[derive(Debug, PartialEq, FromQueryResult)]
    struct Table {
        table_schema: String,
        table_name: String,
    }

    /// an empty database next to the test one
    async fn empty_db(url: &str, name: &str) -> (DatabaseConnection, String) {
        let (server, _) = url.rsplit_once('/').unwrap();
        let admin = Database::connect(format!("{server}/postgres")).await.unwrap();
        admin.execute_unprepared(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)")).await.unwrap();
        admin.execute_unprepared(&format!("CREATE DATABASE {name}")).await.unwrap();
        admin.close().await.unwrap();
        let url = format!("{server}/{name}");
        (Database::connect(&url).await.unwrap(), url)
    }

    async fn tables(conn: &DatabaseConnection) -> Vec<Table> {
        Table::find_by_statement(Statement::from_string(
            conn.get_database_backend(),
            "select table_schema::text, table_name::text from information_schema.tables
            where table_schema not in ('pg_catalog', 'information_schema') order by table_name",
        ))
        .all(conn)
        .await
        .unwrap()
    }

    async fn versions(conn: &DatabaseConnection) -> Vec<String> {
        conn.query_all_raw(Statement::from_string(
            conn.get_database_backend(),
            "select version from seaql_migrations order by version",
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.try_get("", "version").unwrap())
        .collect()
    }

    const VERSIONS: [&str; 2] = ["m20261121_000001_baseline", "m20261121_000002_portal_watches"];

    #[tokio::test]
    async fn fresh_database() {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else { return };
        let (conn, _) = empty_db(&url, "p0sticcio_bot_migrations_fresh").await;

        Migrator::up(&conn, None).await.unwrap();
        // nothing left to do
        Migrator::up(&conn, None).await.unwrap();
        assert_eq!(versions(&conn).await, VERSIONS);
        let names = tables(&conn).await.into_iter().map(|t| t.table_name).collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "locations",
                "portal_mods",
                "portal_resonators",
                "portal_revisions",
                "portal_watches",
                "portals",
                "seaql_migrations"
            ]
        );
    }

    /// production databases were created by hand from schema.sql, in their own schema
    #[tokio::test]
    async fn legacy_database() {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else { return };
        let name = "p0sticcio_bot_migrations_legacy";
        let (conn, url) = empty_db(&url, name).await;
        conn.execute_unprepared(&LEGACY.replace("ALTER DATABASE p0sticcio_bot ", &format!("ALTER DATABASE {name} ")))
            .await
            .unwrap();
        conn.execute_unprepared("insert into p0sticcio_bot.locations values (1, 45.5, 11.25)").await.unwrap();
        // portal_watches could have been created by hand before migrations existed
        conn.execute_unprepared(
            "CREATE TABLE p0sticcio_bot.portal_watches (id BIGSERIAL PRIMARY KEY, latitude DOUBLE PRECISION NOT NULL,
            longitude DOUBLE PRECISION NOT NULL, virus_time BIGINT NOT NULL, faction CHAR(1), ended_at BIGINT);
            CREATE UNIQUE INDEX portal_watches_coords ON p0sticcio_bot.portal_watches (latitude, longitude);",
        )
        .await
        .unwrap();
        conn.close().await.unwrap();

        // the database search_path applies to new connections, like the application ones
        let conn = Database::connect(&url).await.unwrap();
        Migrator::up(&conn, None).await.unwrap();
        assert_eq!(versions(&conn).await, VERSIONS);
        // everything in the application schema, nothing in public
        assert!(tables(&conn).await.iter().all(|t| t.table_schema == "p0sticcio_bot"), "{:?}", tables(&conn).await);
        assert!(tables(&conn).await.iter().any(|t| t.table_name == "portal_watches"));
        let row = conn
            .query_one_raw(Statement::from_string(conn.get_database_backend(), "select count(*) as n from locations"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.try_get::<i64>("", "n").unwrap(), 1);
    }
}
