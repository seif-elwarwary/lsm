pub mod db;
mod memtable;

pub use db::{Db, DbError};
pub use memtable::LookupResult;
