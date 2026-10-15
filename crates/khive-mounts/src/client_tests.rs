use super::*;

#[tokio::test]
async fn expired_queued_job_replies_with_timeout() {
    let (reply, receiver) = oneshot::channel();
    let job = Job {
        operation: Operation::Catalog,
        reply,
        deadline: Instant::now() - Duration::from_millis(1),
    };

    assert!(job.into_ready().is_none());
    let failure = receiver
        .await
        .expect("the worker must reply to a queued job")
        .expect_err("the expired job must be refused");
    assert_eq!(failure.class, "tool_timeout");
    assert_eq!(failure.reason, "deadline_exceeded");
}
