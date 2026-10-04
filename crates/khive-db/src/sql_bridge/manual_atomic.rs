use super::*;

/// Run `op` under a manual `BEGIN IMMEDIATE`/`COMMIT`/`ROLLBACK` on `writer`
/// — the pre-ADR-067 shape, used by [`SqlBridge::atomic_unit`] whenever no
/// writer task applies to a file-backed pool.
pub(super) async fn run_manual_atomic_unit(
    writer: &mut dyn khive_storage::SqlWriter,
    op: AtomicUnitOp,
    origin: khive_storage::tx_registry::TxOrigin,
) -> khive_storage::types::StorageResult<Box<dyn Any + Send>> {
    fn tx_stmt(sql: &str, label: &str) -> SqlStatement {
        SqlStatement {
            sql: sql.to_string(),
            params: vec![],
            label: Some(label.to_string()),
        }
    }
    khive_storage::SqlWriter::execute(writer, tx_stmt("BEGIN IMMEDIATE", "begin")).await?;
    let _tx_handle =
        khive_storage::tx_registry::register_scoped(Some("atomic_unit".to_string()), origin);

    let result = op(writer).await;

    match result {
        Ok(value) => {
            match khive_storage::SqlWriter::execute(writer, tx_stmt("COMMIT", "commit")).await {
                Ok(_) => Ok(value),
                Err(e) => {
                    let _ =
                        khive_storage::SqlWriter::execute(writer, tx_stmt("ROLLBACK", "rollback"))
                            .await;
                    Err(e)
                }
            }
        }
        Err(e) => {
            let _ =
                khive_storage::SqlWriter::execute(writer, tx_stmt("ROLLBACK", "rollback")).await;
            Err(e)
        }
    }
}
