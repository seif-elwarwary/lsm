mod memtable;
pub mod db;

pub use db::{Db, DbError};
pub use memtable::LookupResult;
