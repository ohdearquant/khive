//! Transport allowances for the same parsed request used by dispatch.

use std::time::Duration;

use khive_request::{parse_request, ArgValue, ParsedRequest};
#[cfg(unix)]
use khive_runtime::{classify_operation, OperationAccess, VerbRegistry};

/// Connect/write, scheduling and response serialization allowance beyond the
/// handler's intentional wait. This is not an unbounded slow-peer exemption.
const LONG_POLL_MARGIN: Duration = Duration::from_secs(5);

#[cfg(unix)]
pub(crate) fn read_replay_safe(ops: &str) -> bool {
    parse_request(ops).is_ok_and(|parsed| {
        !parsed.ops.is_empty()
            && parsed.ops.iter().all(|op| {
                classify_operation(&op.tool) == Some(OperationAccess::Read)
                    && !VerbRegistry::SIDE_EFFECTING_ASSERTIVE_VERBS.contains(&op.tool.as_str())
            })
    })
}

pub(crate) fn read_timeout(ops: &str, configured: Duration) -> Duration {
    parse_request(ops).map_or(configured, |parsed| {
        parsed_read_timeout(&parsed, configured)
    })
}

pub(crate) fn parsed_read_timeout(parsed: &ParsedRequest, configured: Duration) -> Duration {
    let max_wait = khive_pack_comm::handlers::MAX_INBOX_WAIT_MS;
    let wait_ms: u64 = parsed
        .ops
        .iter()
        .filter(|op| op.tool == "comm.inbox")
        .map(|op| match op.args.get("wait_ms") {
            Some(ArgValue::Value(value)) => {
                value.as_u64().filter(|ms| *ms <= max_wait).unwrap_or(0)
            }
            // A chain reference is not resolved until dispatch. Reserve the
            // handler's maximum valid wait without trusting the future value.
            Some(ArgValue::PrevRef { .. }) => max_wait,
            _ => 0,
        })
        .sum();
    if wait_ms == 0 {
        configured
    } else {
        // Summing also bounds serial chains and parallel batches that run in
        // bounded waves. The parser admits at most 100 operations: at most
        // 3,000 seconds of intentional waiting plus this five-second margin.
        configured.max(Duration::from_millis(wait_ms) + LONG_POLL_MARGIN)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_poll_allowance_is_bounded_by_the_handler_and_parser_contracts() {
        let base = Duration::from_secs(30);
        for ops in [
            "stats()",
            "comm.inbox()",
            "comm.inbox(wait_ms=0)",
            "comm.inbox(wait_ms=30001)",
            "comm.inbox(wait_ms=null)",
            "invalid(",
        ] {
            assert_eq!(read_timeout(ops, base), base, "{ops}");
        }
        assert_eq!(
            read_timeout("comm.inbox(wait_ms=30000)", base),
            Duration::from_secs(35)
        );
        assert_eq!(
            read_timeout(
                "comm.inbox(wait_ms=30000) | comm.inbox(wait_ms=$prev.wait_ms)",
                base
            ),
            Duration::from_secs(65)
        );
        assert_eq!(
            read_timeout("comm.inbox(wait_ms=30000)", Duration::from_secs(90)),
            Duration::from_secs(90)
        );
        let batch = format!(
            "[{}]",
            vec!["comm.inbox(wait_ms=30000)"; khive_request::MAX_OPS].join(",")
        );
        assert_eq!(read_timeout(&batch, base), Duration::from_secs(3005));
    }

    #[cfg(unix)]
    #[test]
    fn replay_uses_operation_access_for_every_operation() {
        for ops in [
            "comm.inbox()",
            "get(id=\"x\")",
            "[list(), get(id=\"x\")]",
            "stats() | comm.unread()",
        ] {
            assert!(read_replay_safe(ops), "{ops}");
        }
        for ops in [
            "comm.read(id=\"x\")",
            "memory.recall(query=\"x\")",
            "search(query=\"x\")",
            "[list(), search(query=\"x\")]",
            "comm.send(to=\"x\", content=\"x\")",
            "[stats(), comm.mark_read(ids=[])]",
            "stats() | unknown.read()",
            "invalid(",
            "[]",
        ] {
            assert!(!read_replay_safe(ops), "{ops}");
        }
    }
}
