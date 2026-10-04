use super::*;

#[cfg(test)]
crate::writer_busy_fixture::direct_busy_case!(
    direct_busy_with_writer,
    |pool, standalone| async move {
        SqlGraphStore::new_scoped(pool, standalone, "default")
            .with_writer("direct_busy_fixture", crate::writer_busy_fixture::insert)
            .await
    }
);
