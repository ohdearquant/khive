use std::cell::RefCell;

use rusqlite::Connection;

use super::{Budget, DatabasePageAccounting};

type AfterAccounting = Box<dyn FnOnce(&Connection, DatabasePageAccounting) + Send>;

#[derive(Default)]
pub(super) struct Hooks {
    pub(super) budget: Option<Budget>,
    pub(super) metadata_error: bool,
    pub(super) after_accounting: Option<AfterAccounting>,
    pub(super) dbstat_entries: usize,
}

thread_local! {
    static HOOKS: RefCell<Hooks> = RefCell::new(Hooks::default());
}

pub(super) struct Guard(Option<Hooks>);

pub(super) fn install(hooks: Hooks) -> Guard {
    Guard(Some(HOOKS.with(|current| current.replace(hooks))))
}

impl Drop for Guard {
    fn drop(&mut self) {
        HOOKS.with(|current| current.replace(self.0.take().expect("hook guard restored once")));
    }
}

pub(super) fn entries() -> usize {
    HOOKS.with(|hooks| hooks.borrow().dbstat_entries)
}

pub(super) fn budget(default: Budget) -> Budget {
    HOOKS.with(|hooks| hooks.borrow().budget.unwrap_or(default))
}

pub(super) fn accounting_result(
    result: rusqlite::Result<DatabasePageAccounting>,
) -> rusqlite::Result<DatabasePageAccounting> {
    if HOOKS.with(|hooks| hooks.borrow().metadata_error) {
        Err(rusqlite::Error::InvalidParameterName(
            "injected accounting failure".to_owned(),
        ))
    } else {
        result
    }
}

pub(super) fn after_accounting(conn: &Connection, pages: DatabasePageAccounting) {
    let callback = HOOKS.with(|hooks| hooks.borrow_mut().after_accounting.take());
    if let Some(callback) = callback {
        callback(conn, pages);
    }
}

pub(super) fn before_dbstat() {
    HOOKS.with(|hooks| {
        let mut hooks = hooks.borrow_mut();
        hooks.dbstat_entries += 1;
    })
}
