//! Non-migrating read-only transports for ouro's public transactional seam.
//! The engine still owns all row decoding, indexes, queries and inspections.
use async_trait::async_trait;
use ouro_durable::{AtomicBackend, Rows, Snapshot, Transaction};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{sync::Mutex, task::JoinHandle};

pub(crate) enum ReadBackend {
    Sqlite(PathBuf),
    Postgres(Box<tokio_postgres::Config>),
}
impl ReadBackend {
    pub async fn open(spec: &str) -> anyhow::Result<Self> {
        let backend = if let Some(path) = spec.strip_prefix("sqlite:").filter(|p| !p.is_empty()) {
            Self::Sqlite(path.into())
        } else if let Some(url) = spec.strip_prefix("postgres:").filter(|p| !p.is_empty()) {
            Self::Postgres(Box::new(url.parse()?))
        } else {
            anyhow::bail!("store must be sqlite:<path> or postgres:<url>");
        };
        // begin validates the schema without creating/migrating anything.
        backend.begin().await?.rollback().await;
        Ok(backend)
    }
}
struct SqliteRead(Arc<Mutex<Connection>>);
impl SqliteRead {
    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&Connection) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let connection = self.0.clone().lock_owned().await;
        tokio::task::spawn_blocking(move || operation(&connection)).await?
    }
}
struct PostgresRead {
    client: tokio_postgres::Client,
    connection: JoinHandle<()>,
}
impl Drop for PostgresRead {
    fn drop(&mut self) {
        // Closing the connection rolls back even if a query future was cancelled.
        self.connection.abort();
    }
}
#[async_trait]
impl AtomicBackend for ReadBackend {
    async fn begin(&self) -> anyhow::Result<Box<dyn Transaction + '_>> {
        match self {
            Self::Sqlite(path) => {
                let path = path.clone();
                let connection =
                    tokio::task::spawn_blocking(move || -> anyhow::Result<Connection> {
                        let connection =
                            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                        connection.busy_timeout(Duration::from_secs(30))?;
                        connection.execute_batch("BEGIN DEFERRED")?;
                        let version: u32 =
                            connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
                        anyhow::ensure!(
                            version == 1,
                            "read-only SQLite requires initialized schema version 1"
                        );
                        Ok(connection)
                    })
                    .await??;
                Ok(Box::new(SqliteRead(Arc::new(Mutex::new(connection)))))
            }
            Self::Postgres(config) => {
                let (client, connection) = config.connect(tokio_postgres::NoTls).await?;
                let tx = PostgresRead {
                    client,
                    connection: tokio::spawn(async move {
                        let _ = connection.await;
                    }),
                };
                tx.client.batch_execute("BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY; SET LOCAL statement_timeout = '30s'").await?;
                let version: i32 = tx
                    .client
                    .query_one("SELECT COALESCE(MAX(version),0) FROM ouro_migrations", &[])
                    .await?
                    .get(0);
                anyhow::ensure!(
                    version == 1,
                    "read-only Postgres requires initialized schema version 1"
                );
                Ok(Box::new(tx))
            }
        }
    }
    async fn snapshot(&self) -> anyhow::Result<Snapshot> {
        anyhow::bail!("full snapshots are disabled on the console read-only backend")
    }
}
#[async_trait]
impl Transaction for SqliteRead {
    async fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<Value>> {
        let (kind, id) = (kind.to_owned(), id.to_owned());
        self.run(move |c| {
            let raw: Option<String> = c
                .query_row(
                    "SELECT value FROM ouro_rows WHERE kind=?1 AND id=?2",
                    params![kind, id],
                    |r| r.get(0),
                )
                .optional()?;
            raw.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
        })
        .await
    }
    async fn scan(
        &mut self,
        kind: &str,
        start: &str,
        end: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Rows> {
        let (kind, start, end) = (kind.to_owned(), start.to_owned(), end.map(str::to_owned));
        let limit = i64::try_from(limit)?;
        self.run(move |c| {
            let mut query = c.prepare("SELECT id,value FROM ouro_rows WHERE kind=?1 AND id>=?2 AND (?3 IS NULL OR id<?3) ORDER BY id LIMIT ?4")?;
            let rows = query.query_map(params![kind, start, end, limit], |r| Ok((r.get::<_,String>(0)?, r.get::<_,String>(1)?)))?;
            rows.map(|r| { let (id, v) = r?; Ok(((kind.clone(), id), serde_json::from_str(&v)?)) }).collect()
        }).await
    }
    async fn put(&mut self, _: &str, _: &str, _: &Value) -> anyhow::Result<()> {
        anyhow::bail!("read-only backend")
    }
    async fn delete(&mut self, _: &str, _: &str) -> anyhow::Result<()> {
        anyhow::bail!("read-only backend")
    }
    async fn commit(self: Box<Self>) -> anyhow::Result<()> {
        self.run(|c| Ok(c.execute_batch("ROLLBACK")?)).await
    }
    // Dropping the only connection also rolls back on cancellation/error.
}
#[async_trait]
impl Transaction for PostgresRead {
    async fn get(&mut self, kind: &str, id: &str) -> anyhow::Result<Option<Value>> {
        self.client
            .query_opt(
                "SELECT value FROM ouro_rows WHERE kind=$1 AND id=$2",
                &[&kind, &id],
            )
            .await?
            .map(|r| Ok(serde_json::from_str(&r.get::<_, String>(0))?))
            .transpose()
    }
    async fn scan(
        &mut self,
        kind: &str,
        start: &str,
        end: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Rows> {
        let limit = i64::try_from(limit)?;
        self.client.query("SELECT id,value FROM ouro_rows WHERE kind=$1 AND id>=$2 AND ($3::text IS NULL OR id<$3) ORDER BY id LIMIT $4", &[&kind, &start, &end, &limit]).await?
            .into_iter().map(|r| Ok(((kind.into(), r.get(0)), serde_json::from_str(&r.get::<_,String>(1))?))).collect()
    }
    async fn put(&mut self, _: &str, _: &str, _: &Value) -> anyhow::Result<()> {
        anyhow::bail!("read-only backend")
    }
    async fn delete(&mut self, _: &str, _: &str) -> anyhow::Result<()> {
        anyhow::bail!("read-only backend")
    }
    async fn commit(self: Box<Self>) -> anyhow::Result<()> {
        Ok(self.client.batch_execute("ROLLBACK").await?)
    }
}
