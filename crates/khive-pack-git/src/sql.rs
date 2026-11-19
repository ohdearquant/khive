//! Static SQL assets retain their compile-time `sql!` loading. The seven entity
//! lookups use a closed typed selector with fixed clauses and bound values.

mod entity_rows;
pub(crate) use entity_rows::EntityRows;
pub(crate) use khive_runtime::sql;
