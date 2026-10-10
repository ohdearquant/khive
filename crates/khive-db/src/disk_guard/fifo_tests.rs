use super::*;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

const OBSERVE: Duration = Duration::from_secs(10);

fn key(which: u64) -> ProcessSlotKey {
    ProcessSlotKey {
        #[cfg(unix)]
        volume: VolumeKey::UnixDevice(u64::MAX - which),
        #[cfg(windows)]
        volume: VolumeKey::WindowsSerial(u32::MAX - which as u32),
        harness_namespace: None,
    }
}

fn queued(key: &ProcessSlotKey) -> Vec<ThreadId> {
    process_registry()
        .slots
        .lock()
        .get(key)
        .map(|slot| slot.waiters.iter().copied().collect())
        .unwrap_or_default()
}

fn await_queued(key: &ProcessSlotKey, id: ThreadId) {
    let deadline = Instant::now() + OBSERVE;
    loop {
        if queued(key).contains(&id) {
            return;
        }
        assert!(Instant::now() < deadline, "waiter never entered the queue");
        // Observe the actual protected queue. This delay never determines
        // arrival order; the next thread is launched only after observation.
        std::thread::sleep(Duration::from_millis(1));
    }
}

struct Waiter {
    id: ThreadId,
    release: Sender<()>,
    join: JoinHandle<Result<(), LeaseRefusal>>,
}

fn waiter(
    key: &ProcessSlotKey,
    number: usize,
    timeout: Duration,
    acquired: &Sender<usize>,
) -> Waiter {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let owned_key = key.clone();
    let acquired = acquired.clone();
    let join = std::thread::spawn(move || {
        let deadline = Instant::now() + timeout;
        entered_tx.send(std::thread::current().id()).unwrap();
        let held = acquire_process_slot(owned_key, deadline, Location::caller())?;
        acquired.send(number).unwrap();
        released.recv_timeout(OBSERVE).expect("release held waiter");
        drop(held);
        Ok(())
    });
    let id = entered_rx.recv_timeout(OBSERVE).unwrap();
    await_queued(key, id);
    Waiter { id, release, join }
}

fn next(acquired: &Receiver<usize>, number: usize, waiter: Waiter) {
    assert_eq!(acquired.recv_timeout(OBSERVE).unwrap(), number);
    assert!(
        acquired.try_recv().is_err(),
        "only one slot holder at a time"
    );
    waiter.release.send(()).unwrap();
    waiter.join.join().unwrap().unwrap();
}

fn assert_timeout(error: &LeaseRefusal) {
    assert!(
        matches!(
            error,
            LeaseRefusal::TimedOut(SqliteError::CapacityUnavailable {
                phase: CapacityUnavailablePhase::Lock,
                message,
            }) if message == "timed out waiting for in-process volume lease"
        ),
        "unexpected refusal: {error:?}"
    );
}

fn assert_empty(key: &ProcessSlotKey) {
    assert!(!process_registry().slots.lock().contains_key(key));
}

#[test]
fn queued_waiters_keep_arrival_order_ahead_of_a_later_request() {
    let key = key(101);
    let held =
        acquire_process_slot(key.clone(), Instant::now() + OBSERVE, Location::caller()).unwrap();
    let (tx, rx) = mpsc::channel();
    let first = waiter(&key, 0, OBSERVE, &tx);
    let second = waiter(&key, 1, OBSERVE, &tx);
    let third = waiter(&key, 2, OBSERVE, &tx);
    assert_eq!(queued(&key), [first.id, second.id, third.id]);
    drop(held);
    // The first waiter cannot release until this test tells it to, so this
    // arrival is observable even if it races the first grant after release.
    let later = waiter(&key, 3, OBSERVE, &tx);
    next(&rx, 0, first);
    next(&rx, 1, second);
    next(&rx, 2, third);
    next(&rx, 3, later);
    assert_empty(&key);
}

#[test]
fn expired_head_is_removed_and_its_successor_can_acquire() {
    let key = key(102);
    let held =
        acquire_process_slot(key.clone(), Instant::now() + OBSERVE, Location::caller()).unwrap();
    let (tx, rx) = mpsc::channel();
    let head = waiter(&key, 0, Duration::from_secs(2), &tx);
    let successor = waiter(&key, 1, OBSERVE, &tx);
    assert_eq!(queued(&key), [head.id, successor.id]);
    let error = head
        .join
        .join()
        .unwrap()
        .expect_err("head times out while held");
    assert_timeout(&error);
    assert_eq!(queued(&key), [successor.id]);
    assert!(rx.try_recv().is_err());
    drop(held);
    next(&rx, 1, successor);
    assert_empty(&key);
}

#[test]
fn expired_middle_preserves_the_order_of_remaining_waiters() {
    let key = key(103);
    let held =
        acquire_process_slot(key.clone(), Instant::now() + OBSERVE, Location::caller()).unwrap();
    let (tx, rx) = mpsc::channel();
    let first = waiter(&key, 0, OBSERVE, &tx);
    let middle = waiter(&key, 1, Duration::from_secs(2), &tx);
    let last = waiter(&key, 2, OBSERVE, &tx);
    assert_eq!(queued(&key), [first.id, middle.id, last.id]);
    let error = middle
        .join
        .join()
        .unwrap()
        .expect_err("middle times out while held");
    assert_timeout(&error);
    assert_eq!(queued(&key), [first.id, last.id]);
    drop(held);
    next(&rx, 0, first);
    next(&rx, 2, last);
    assert_empty(&key);
}

#[test]
fn reentry_and_expired_arrival_do_not_change_the_waiting_queue() {
    let key = key(104);
    let held =
        acquire_process_slot(key.clone(), Instant::now() + OBSERVE, Location::caller()).unwrap();
    let (tx, rx) = mpsc::channel();
    let follower = waiter(&key, 0, OBSERVE, &tx);
    let error = acquire_process_slot(key.clone(), Instant::now() + OBSERVE, Location::caller())
        .err()
        .expect("same-thread re-entry refuses");
    match error {
        LeaseRefusal::Refused(SqliteError::VolumeLeaseReentry {
            holder_site,
            requester_site,
        }) => {
            assert!(holder_site.contains("fifo_tests.rs"));
            assert!(requester_site.contains("fifo_tests.rs"));
            assert_ne!(holder_site, requester_site);
        }
        other => panic!("re-entry must remain a distinct refusal: {other:?}"),
    }
    let error = acquire_process_slot(key.clone(), Instant::now(), Location::caller())
        .err()
        .expect("expired deadline retains precedence over re-entry");
    assert_timeout(&error);
    assert_eq!(queued(&key), [follower.id]);
    drop(held);
    next(&rx, 0, follower);
    assert_empty(&key);
    let error = acquire_process_slot(key.clone(), Instant::now(), Location::caller())
        .err()
        .expect("expired deadline cannot acquire an empty slot");
    assert_timeout(&error);
    assert_empty(&key);
}

#[test]
fn detached_volume_lease_preserves_the_queue_until_cross_thread_release() {
    let dir = tempfile::tempdir().unwrap();
    let mut identity = VolumeIdentity::resolve(&dir.path().join("db.sqlite")).unwrap();
    identity.key = key(105).volume;
    let locks = dir.path().join("locks");
    let slot_key = ProcessSlotKey::new(identity.key, &locks);
    let held = identity.acquire_classified(OBSERVE, &locks).unwrap();
    let detached = held.detach_from_thread();
    let error = identity
        .acquire_classified(Duration::from_millis(30), &locks)
        .err()
        .expect("original thread must contend after detach, not re-enter");
    assert_timeout(&error);
    let (entered_tx, entered_rx) = mpsc::channel();
    let (acquired_tx, acquired_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let next_identity = identity.clone();
    let next_locks = locks.clone();
    let follower = std::thread::spawn(move || {
        entered_tx.send(std::thread::current().id()).unwrap();
        let held = next_identity
            .acquire_classified(OBSERVE, &next_locks)
            .unwrap();
        acquired_tx.send(()).unwrap();
        release_rx.recv_timeout(OBSERVE).unwrap();
        drop(held);
    });
    let id = entered_rx.recv_timeout(OBSERVE).unwrap();
    await_queued(&slot_key, id);
    assert!(acquired_rx.try_recv().is_err());
    std::thread::spawn(move || drop(detached)).join().unwrap();
    acquired_rx.recv_timeout(OBSERVE).unwrap();
    release_tx.send(()).unwrap();
    follower.join().unwrap();
    assert_empty(&slot_key);
    drop(identity.acquire_classified(OBSERVE, &locks).unwrap());
    assert_empty(&slot_key);
}
