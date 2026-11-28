mod counts;
mod grouping;
mod page;
mod parsing;
mod recent;

#[cfg(any(test, feature = "test-internals"))]
#[doc(hidden)]
pub mod test_support_exports {
    pub use super::counts::{
        fetch_event_counts_window, fetch_event_counts_window_exhaustive, TRANSPORT_PAGE_ROWS,
    };
    pub use super::grouping::{EventCountDimension, EventCountGroupBy, EventCountsAccumulator};
}

#[cfg(test)]
mod counts_tests;

#[cfg(test)]
mod fold_tests;

#[cfg(test)]
mod fold_controls;

#[cfg(test)]
mod group_tests;

#[cfg(test)]
mod error_tests;

#[cfg(test)]
mod sampling_tests;

#[cfg(test)]
mod read_budget_tests;

#[cfg(test)]
mod page_tests;

#[cfg(test)]
mod test_support;
