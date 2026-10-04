use super::*;

#[cfg(test)]
crate::writer_busy_fixture::direct_busy_case!(
    direct_busy_graph_composition_begin,
    |pool: std::sync::Arc<ConnectionPool>, standalone| async move {
        tokio::task::spawn_blocking(move || {
            if standalone {
                let conn = pool.open_standalone_writer().unwrap();
                run_graph_mutation_transaction(&pool, &conn, false, |conn| {
                    crate::writer_busy_fixture::insert(conn)
                        .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))
                })
            } else {
                let guard = pool.try_writer().unwrap();
                run_graph_mutation_transaction(&pool, guard.conn(), true, |conn| {
                    crate::writer_busy_fixture::insert(conn)
                        .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))
                })
            }
        })
        .await
        .unwrap()
    }
);

#[cfg(test)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_busy_graph_composition_body_error_counts_once() {
    crate::writer_busy_fixture::assert_transaction_body_busy(|pool, path| async move {
        tokio::task::spawn_blocking(move || {
            let guard = pool.try_writer().unwrap();
            run_graph_mutation_transaction(&pool, guard.conn(), true, |conn| {
                crate::writer_busy_fixture::insert_blocked(conn, &path)
                    .map_err(|error| map_err(error, GRAPH_MUTATION_EVENTS_OP))
            })
        })
        .await
        .unwrap()
    })
    .await;
}
