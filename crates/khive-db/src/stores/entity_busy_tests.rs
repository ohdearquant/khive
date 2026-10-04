use super::*;

#[cfg(test)]
crate::writer_busy_fixture::direct_busy_case!(
    direct_busy_single_writer,
    |pool, standalone| async move {
        SqlEntityStore::new(pool, standalone)
            .with_writer("direct_busy_fixture", crate::writer_busy_fixture::insert)
            .await
    }
);

#[cfg(test)]
crate::writer_busy_fixture::direct_busy_case!(
    direct_busy_with_writer_tx,
    |pool, standalone| async move {
        SqlEntityStore::new(pool, standalone)
            .with_writer_tx("direct_busy_fixture", crate::writer_busy_fixture::insert)
            .await
    }
);

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_locked_constraint_and_checkout_refusals_add_zero() {
    let fixture = crate::writer_busy_fixture::Fixture::new(true, false);
    let store = SqlEntityStore::new(Arc::clone(&fixture.pool), true);
    store
        .with_writer("fixture_insert", crate::writer_busy_fixture::insert)
        .await
        .unwrap();
    let locked = store
        .with_writer("fixture_locked", |conn| {
            conn.execute("INSERT INTO direct_busy_fixture VALUES (2)", [])?;
            let mut statement = conn.prepare("SELECT * FROM direct_busy_fixture")?;
            let mut rows = statement.query([])?;
            assert!(rows.next()?.is_some());
            conn.execute_batch("DROP TABLE direct_busy_fixture")
        })
        .await
        .unwrap_err();
    assert_eq!(
        crate::read_cancellation::storage_error_sqlite_code(&locked),
        Some(rusqlite::ErrorCode::DatabaseLocked)
    );
    let constraint = store
        .with_writer("fixture_constraint", |conn| {
            conn.execute("INSERT INTO direct_busy_fixture VALUES (1)", [])
                .map(|_| ())
        })
        .await
        .unwrap_err();
    assert_eq!(
        crate::read_cancellation::storage_error_sqlite_code(&constraint),
        Some(rusqlite::ErrorCode::ConstraintViolation)
    );
    assert_eq!(
        fixture
            .pool
            .writer_acquisition_snapshot()
            .direct_busy_refusals,
        0
    );
    let held = fixture.pool.try_writer().unwrap();
    let refused = store
        .with_writer("fixture_checkout", crate::writer_busy_fixture::insert)
        .await
        .unwrap_err();
    assert!(matches!(refused, StorageError::Driver { .. }));
    assert_eq!(fixture.pool.writer_acquisition_snapshot().timeouts, 1);
    assert_eq!(
        fixture
            .pool
            .writer_acquisition_snapshot()
            .direct_busy_refusals,
        0
    );
    drop(held);
    store
        .with_writer("fixture_recover", crate::writer_busy_fixture::insert)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .pool
            .writer_acquisition_snapshot()
            .direct_busy_refusals,
        0
    );
}

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_busy_entity_transaction_body_error_counts_once() {
    crate::writer_busy_fixture::assert_transaction_body_busy(|pool, path| async move {
        SqlEntityStore::new(pool, true)
            .with_writer_tx("fixture_body", move |conn| {
                crate::writer_busy_fixture::insert_blocked(conn, &path)
            })
            .await
    })
    .await;
}
