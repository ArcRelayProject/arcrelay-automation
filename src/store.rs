use crate::*;
use chrono::{Duration, Utc};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    Row, SqlitePool,
};
use std::{str::FromStr, sync::Arc};
use tokio::sync::OnceCell;

#[derive(Clone)]
pub struct AutomationStore {
    options: SqliteConnectOptions,
    pool: Arc<OnceCell<SqlitePool>>,
}
impl AutomationStore {
    /// Validate configuration without opening SQLite or requiring a Tokio runtime.
    /// The shared pool and schema are initialized on first asynchronous use;
    /// unsuccessful initialization remains retryable.
    pub fn new(url: &str) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(url)?
            .create_if_missing(true)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        Ok(Self {
            options,
            pool: Arc::new(OnceCell::new()),
        })
    }
    async fn db(&self) -> Result<&SqlitePool> {
        self.pool.get_or_try_init(|| async {
            // Even connect_lazy_with starts SQLx maintenance tasks immediately.
            // Create the pool here, on the runtime that will actually use it,
            // never in the synchronous desktop preparation phase.
            let pool = SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(self.options.clone())
                .await?;
            sqlx::raw_sql("CREATE TABLE IF NOT EXISTS automations (id TEXT PRIMARY KEY, revision INTEGER NOT NULL, definition TEXT NOT NULL, legacy_id TEXT UNIQUE);
                CREATE TABLE IF NOT EXISTS automation_runs (id TEXT PRIMARY KEY, automation_id TEXT NOT NULL, event_id TEXT NOT NULL, status TEXT NOT NULL, created_at INTEGER NOT NULL, data TEXT NOT NULL, UNIQUE(automation_id,event_id));
                CREATE INDEX IF NOT EXISTS automation_activity_time ON automation_runs(automation_id,created_at DESC);
                CREATE TABLE IF NOT EXISTS automation_step_runs (run_id TEXT NOT NULL REFERENCES automation_runs(id) ON DELETE CASCADE, step_index INTEGER NOT NULL, data TEXT NOT NULL, PRIMARY KEY(run_id,step_index));
                CREATE TABLE IF NOT EXISTS automation_schedule (automation_id TEXT PRIMARY KEY, next_at INTEGER NOT NULL);
                CREATE TABLE IF NOT EXISTS automation_migrations (legacy_id TEXT PRIMARY KEY);").execute(&pool).await?;
            Ok::<_, AutomationError>(pool)
        }).await
    }
    pub async fn list(&self) -> Result<Vec<AutomationDefinition>> {
        sqlx::query_scalar::<_, String>("SELECT definition FROM automations ORDER BY rowid DESC")
            .fetch_all(self.db().await?)
            .await?
            .iter()
            .map(|s| Ok(serde_json::from_str(s)?))
            .collect()
    }
    pub async fn get(&self, id: &str) -> Result<AutomationDefinition> {
        let json: String = sqlx::query_scalar("SELECT definition FROM automations WHERE id=?")
            .bind(id)
            .fetch_optional(self.db().await?)
            .await?
            .ok_or(AutomationError::NotFound)?;
        Ok(serde_json::from_str(&json)?)
    }
    pub async fn save(&self, mut definition: AutomationDefinition) -> Result<AutomationDefinition> {
        validate(&definition)?;
        let mut tx = self.db().await?.begin().await?;
        let old: Option<String> =
            sqlx::query_scalar("SELECT definition FROM automations WHERE id=?")
                .bind(&definition.id)
                .fetch_optional(&mut *tx)
                .await?;
        match old {
            Some(json) => {
                let previous: AutomationDefinition = serde_json::from_str(&json)?;
                if previous.revision != definition.revision {
                    return Err(AutomationError::Conflict);
                }
                definition.created_at = previous.created_at;
                definition.legacy_id = previous.legacy_id;
            }
            None if definition.revision != 0 => return Err(AutomationError::Conflict),
            None => definition.created_at = Utc::now(),
        }
        definition.revision += 1;
        definition.updated_at = Utc::now();
        sqlx::query("INSERT INTO automations(id,revision,definition,legacy_id) VALUES (?,?,?,?) ON CONFLICT(id) DO UPDATE SET revision=excluded.revision,definition=excluded.definition")
            .bind(&definition.id).bind(definition.revision as i64).bind(serde_json::to_string(&definition)?).bind(&definition.legacy_id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM automation_schedule WHERE automation_id=?")
            .bind(&definition.id)
            .execute(&mut *tx)
            .await?;
        if definition.enabled {
            if let Some(next) = next_schedule(&definition.trigger, Utc::now()) {
                sqlx::query("INSERT INTO automation_schedule VALUES (?,?)")
                    .bind(&definition.id)
                    .bind(next.timestamp())
                    .execute(&mut *tx)
                    .await?;
            }
        }
        if let Some(id) = &definition.legacy_id {
            sqlx::query("INSERT OR IGNORE INTO automation_migrations VALUES (?)")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(definition)
    }
    pub async fn migrated(&self, id: &str) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, String>(
            "SELECT legacy_id FROM automation_migrations WHERE legacy_id=?",
        )
        .bind(id)
        .fetch_optional(self.db().await?)
        .await?
        .is_some())
    }
    pub async fn delete(&self, id: &str) -> Result<()> {
        let mut tx = self.db().await?.begin().await?;
        sqlx::query("DELETE FROM automations WHERE id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM automation_schedule WHERE automation_id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn insert_activity(&self, activity: &AutomationActivity) -> Result<bool> {
        Ok(sqlx::query("INSERT OR IGNORE INTO automation_runs(id,automation_id,event_id,status,created_at,data) VALUES (?,?,?,?,?,?)")
            .bind(&activity.id).bind(&activity.automation_id).bind(&activity.event.id).bind(activity.status.key()).bind(activity.created_at.timestamp_millis()).bind(serde_json::to_string(activity)?).execute(self.db().await?).await?.rows_affected() == 1)
    }
    pub async fn save_activity(&self, activity: &AutomationActivity) -> Result<()> {
        let mut tx = self.db().await?.begin().await?;
        sqlx::query("UPDATE automation_runs SET status=?,data=? WHERE id=?")
            .bind(activity.status.key())
            .bind(serde_json::to_string(activity)?)
            .bind(&activity.id)
            .execute(&mut *tx)
            .await?;
        for step in &activity.steps {
            sqlx::query("INSERT INTO automation_step_runs VALUES (?,?,?) ON CONFLICT(run_id,step_index) DO UPDATE SET data=excluded.data").bind(&activity.id).bind(step.index as i64).bind(serde_json::to_string(step)?).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn activity(&self, id: &str) -> Result<AutomationActivity> {
        let json: String = sqlx::query_scalar("SELECT data FROM automation_runs WHERE id=?")
            .bind(id)
            .fetch_optional(self.db().await?)
            .await?
            .ok_or(AutomationError::NotFound)?;
        Ok(serde_json::from_str(&json)?)
    }
    pub async fn activities(
        &self,
        id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<AutomationActivity>> {
        let data = sqlx::query_scalar::<_, String>("SELECT data FROM automation_runs WHERE (? IS NULL OR automation_id=?) ORDER BY created_at DESC,rowid DESC LIMIT ?").bind(id).bind(id).bind(limit.clamp(1,100)).fetch_all(self.db().await?).await?;
        data.iter()
            .map(|json| Ok(serde_json::from_str(json)?))
            .collect()
    }
    pub async fn active(&self) -> Result<Vec<AutomationActivity>> {
        let data = sqlx::query_scalar::<_, String>("SELECT data FROM automation_runs WHERE status IN ('queued','running','awaitingConfirmation')").fetch_all(self.db().await?).await?;
        data.iter()
            .map(|json| Ok(serde_json::from_str(json)?))
            .collect()
    }
    pub async fn prune(&self, clear: bool) -> Result<()> {
        let cutoff = (Utc::now() - Duration::days(30)).timestamp_millis();
        sqlx::query("DELETE FROM automation_runs WHERE status NOT IN ('queued','running','awaitingConfirmation') AND (? OR created_at < ? OR id NOT IN (SELECT id FROM automation_runs ORDER BY created_at DESC LIMIT 100))")
            .bind(clear).bind(cutoff).execute(self.db().await?).await?;
        Ok(())
    }
    pub async fn due(&self) -> Result<Vec<(String, i64)>> {
        Ok(
            sqlx::query("SELECT automation_id,next_at FROM automation_schedule WHERE next_at<=?")
                .bind(Utc::now().timestamp())
                .fetch_all(self.db().await?)
                .await?
                .iter()
                .map(|r| (r.get(0), r.get(1)))
                .collect(),
        )
    }
    pub async fn advance_schedule(&self, id: &str, next: i64) -> Result<()> {
        sqlx::query("UPDATE automation_schedule SET next_at=? WHERE automation_id=?")
            .bind(next)
            .bind(id)
            .execute(self.db().await?)
            .await?;
        Ok(())
    }
}
