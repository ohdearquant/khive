use super::*;

#[cfg(test)]
crate::writer_busy_fixture::direct_busy_case!(
    direct_busy_single_writer,
    |pool, standalone| async move {
        SqlNoteStore::new(pool, standalone)
            .with_writer("direct_busy_fixture", crate::writer_busy_fixture::insert)
            .await
    }
);

#[cfg(test)]
crate::writer_busy_fixture::direct_busy_case!(
    direct_busy_with_writer_tx,
    |pool, standalone| async move {
        SqlNoteStore::new(pool, standalone)
            .with_writer_tx("direct_busy_fixture", crate::writer_busy_fixture::insert)
            .await
    }
);

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_busy_note_wrapped_transaction_body_error_counts_once() {
    crate::writer_busy_fixture::assert_transaction_body_busy(|pool, path| async move {
        SqlNoteStore::new(pool, true)
            .with_writer_tx_storage("fixture_wrapped_body", move |conn| {
                crate::writer_busy_fixture::insert_blocked(conn, &path).map_err(|error| {
                    StorageError::driver(
                        StorageCapability::Notes,
                        "fixture_wrapped_body",
                        SqliteError::Rusqlite(error),
                    )
                })
            })
            .await
    })
    .await;
}
