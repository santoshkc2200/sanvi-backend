//! TASK-023 operator commands: the platform outbox dead-letter queue.
//! `dlq list` triages what the dispatcher gave up on; `dlq replay`
//! moves a row back to the outbox with a fresh attempt budget once the
//! root cause is fixed. All commands run under the audited platform scope
//! with the `platform-admin` CLI actor.

use std::sync::Arc;

use sanvi_kernel::RawIdGenerator;
use sanvi_persistence::TxManager;

/// Load configuration and connect (migrator role: the app role holds
/// INSERT-only on the DLQ table, so triage and replay run privileged).
async fn connect(environment: &str) -> anyhow::Result<sanvi_persistence::PgPool> {
    let config = crate::config::load(environment)?;
    let pool = sanvi_persistence::build_pool(&config.database).await?;
    Ok(pool)
}

/// `dlq list` — the dead-letter head, newest first.
pub async fn list(environment: &str, limit: i64) -> anyhow::Result<()> {
    let pool = connect(environment).await?;
    let rows = sanvi_messaging::list_dead_letters(&pool, limit).await?;
    if rows.is_empty() {
        println!("no dead-lettered outbox rows");
        return Ok(());
    }
    for row in &rows {
        println!(
            "{} {} {} attempts={} dead_lettered_at={} failure={}",
            row.id, row.aggregate, row.event_type, row.attempts, row.dead_lettered_at, row.failure
        );
    }
    Ok(())
}

/// `dlq replay <event-id>` — requeue one dead-lettered row after fixing
/// the root cause. A poison row never blocks the queue; this is how it
/// gets back in once it is no longer poison.
pub async fn replay(
    environment: &str,
    event_id: i64,
    ids: Arc<dyn RawIdGenerator>,
) -> anyhow::Result<()> {
    let pool = connect(environment).await?;
    let tx = TxManager::new(pool, ids);
    let mut scope = tx
        .begin_unscoped("replay_outbox_dlq", "platform-admin")
        .await?;
    let requeued = sanvi_messaging::replay_dead_letter(&mut scope, event_id).await?;
    scope.commit().await?;
    if requeued {
        println!("requeued {event_id}");
        Ok(())
    } else {
        anyhow::bail!("{event_id} is not dead-lettered (nothing to replay)");
    }
}
