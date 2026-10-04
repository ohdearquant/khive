use super::*;

#[cfg(test)]
crate::writer_busy_fixture::direct_busy_case!(
    direct_busy_with_writer,
    |pool, standalone| async move {
        SqliteVecStore::new(
            pool,
            standalone,
            "fixture".to_string(),
            "fixture".to_string(),
            3,
            "default".to_string(),
        )
        .unwrap()
        .with_writer("direct_busy_fixture", crate::writer_busy_fixture::insert)
        .await
    }
);
