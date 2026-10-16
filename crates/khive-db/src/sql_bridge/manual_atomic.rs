use super::*;

/// Run `op` under a manual `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` on `writer`
/// — the pre-ADR-067 shape, used by [`SqlBridge::atomic_unit`] whenever no
/// writer task applies to a file-backed pool.
pub(super) async fn run_manual_atomic_unit(
    writer: &mut dyn khive_storage::SqlWriter,
    op: AtomicUnitOp,
    origin: khive_storage::tx_registry::TxOrigin,
) -> khive_storage::types::StorageResult<Box<dyn Any + Send>> {
    khive_storage::SqlWriter::execute(
        writer,
        SqlStatement::new("BEGIN IMMEDIATE", vec![]).labelled("begin"),
    )
    .await?;
    let _tx_handle =
        khive_storage::tx_registry::register_scoped(Some("atomic_unit".to_string()), origin);

    let result = op(writer).await;

    match result {
        Ok(value) => {
            match khive_storage::SqlWriter::execute(
                writer,
                SqlStatement::new("COMMIT", vec![]).labelled("commit"),
            )
            .await
            {
                Ok(_) => Ok(value),
                Err(e) => {
                    let _ = khive_storage::SqlWriter::execute(
                        writer,
                        SqlStatement::new("ROLLBACK", vec![]).labelled("rollback"),
                    )
                    .await;
                    Err(e)
                }
            }
        }
        Err(e) => {
            let _ = khive_storage::SqlWriter::execute(
                writer,
                SqlStatement::new("ROLLBACK", vec![]).labelled("rollback"),
            )
            .await;
            Err(e)
        }
    }
}
