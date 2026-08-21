#![warn(rust_2018_idioms)]
#![warn(clippy::all)]

mod canceller;
mod catalog;
mod cmd;
mod connection;
mod cursor;
mod driver;
mod error;
mod value;

pub use canceller::RedisCanceller;
pub use catalog::RedisCatalog;
pub use connection::RedisConnection;
pub use cursor::{ListCursor, OneShotCursor, RedisPairsCursor, ScanFamily, StreamCursor};
pub use driver::RedisDriver;
pub use error::map_redis_error;
