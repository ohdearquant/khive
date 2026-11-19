use rusqlite::{Connection, Transaction, TransactionBehavior};

use super::{
    database_page_accounting, database_size_composition, split_page_accounting_result,
    DatabasePageAccounting, DatabaseSizeComposition, SIZE_COMPOSITION_NOT_REQUESTED,
};

/// Admission limits the ordinary, well-formed snapshot footprint dbstat may
/// traverse. It is not a latency bound or an interruption of blocked OS I/O.
const MAX_COMPOSITION_PAGES: u64 = 8_192;
const MAX_COMPOSITION_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Budget {
    pages: u64,
    bytes: u64,
}

const BUDGET: Budget = Budget {
    pages: MAX_COMPOSITION_PAGES,
    bytes: MAX_COMPOSITION_BYTES,
};

impl Budget {
    fn admits(self, pages: DatabasePageAccounting) -> bool {
        let bytes = pages.page_count.checked_mul(pages.page_size_bytes);
        pages.page_size_bytes.is_power_of_two()
            && (512..=65_536).contains(&pages.page_size_bytes)
            && pages.freelist_pages <= pages.page_count
            && pages.page_count <= self.pages
            && bytes.is_some_and(|bytes| bytes <= self.bytes)
    }

    fn refusal(self, pages: DatabasePageAccounting) -> String {
        let bytes = pages
            .page_count
            .checked_mul(pages.page_size_bytes)
            .map_or_else(|| "overflow".to_owned(), |bytes| bytes.to_string());
        format!(
            "not collected: size composition footprint admission refused \
             (page_count={}, page_size={}, bytes={bytes}; \
             max_pages={}, max_bytes={}); invalid accounting or a limit exceeded",
            pages.page_count, pages.page_size_bytes, self.pages, self.bytes
        )
    }
}

#[derive(Default)]
pub(super) struct PageInspection {
    pub(super) page_accounting: Option<DatabasePageAccounting>,
    pub(super) page_accounting_error: Option<String>,
    pub(super) size_composition: Option<DatabaseSizeComposition>,
    pub(super) size_composition_error: Option<String>,
}

pub(super) fn collect(conn: &Connection, requested: bool) -> PageInspection {
    let transaction = match Transaction::new_unchecked(conn, TransactionBehavior::Deferred) {
        Ok(transaction) => transaction,
        Err(error) => {
            let reason = format!("page accounting snapshot failed: {error}");
            return PageInspection {
                page_accounting_error: Some(reason.clone()),
                size_composition_error: Some(if requested {
                    reason
                } else {
                    SIZE_COMPOSITION_NOT_REQUESTED.to_owned()
                }),
                ..PageInspection::default()
            };
        }
    };
    let mut inspection = inspect_snapshot(&transaction, requested);
    // The cursor is gone before rollback. Drop also rolls back on unwinding;
    // an explicit failure must not present a successful coherent composition.
    if let Err(error) = transaction.rollback() {
        inspection.size_composition = None;
        inspection.size_composition_error =
            Some(format!("page accounting snapshot cleanup failed: {error}"));
    }
    inspection
}

fn inspect_snapshot(conn: &Connection, requested: bool) -> PageInspection {
    let accounting = database_page_accounting(conn);
    #[cfg(test)]
    let accounting = test_support::accounting_result(accounting);
    let (page_accounting, page_accounting_error) = split_page_accounting_result(accounting);
    let mut inspection = PageInspection {
        page_accounting,
        page_accounting_error,
        ..PageInspection::default()
    };
    if !requested {
        inspection.size_composition_error = Some(SIZE_COMPOSITION_NOT_REQUESTED.to_owned());
        return inspection;
    }
    let Some(pages) = page_accounting else {
        inspection.size_composition_error =
            Some("not collected: size composition requires valid page accounting".to_owned());
        return inspection;
    };
    #[cfg(test)]
    test_support::after_accounting(conn, pages);
    let budget = BUDGET;
    #[cfg(test)]
    let budget = test_support::budget(budget);
    if !budget.admits(pages) {
        inspection.size_composition_error = Some(budget.refusal(pages));
        return inspection;
    }
    match database_size_composition(conn, pages) {
        Ok(composition) => inspection.size_composition = Some(composition),
        Err(error) => {
            inspection.size_composition_error =
                Some(format!("database size composition query failed: {error}"));
        }
    }
    inspection
}

#[cfg(test)]
pub(super) fn before_dbstat() {
    test_support::before_dbstat()
}

#[cfg(test)]
#[path = "size_composition_test_support.rs"]
mod test_support;

#[cfg(test)]
#[path = "size_composition_tests.rs"]
mod tests;
