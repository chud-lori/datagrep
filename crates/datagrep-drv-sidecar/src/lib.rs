#![warn(rust_2018_idioms)]
#![warn(clippy::all)]

mod canceller;
mod catalog;
mod connection;
mod cursor;
mod driver;
pub mod frame;
pub mod manifest;
mod process;
pub mod wire;

pub use canceller::SidecarCanceller;
pub use catalog::SidecarCatalog;
pub use connection::SidecarConnection;
pub use cursor::SidecarCursor;
pub use driver::{parse_url, SidecarDriver};
pub use manifest::{manifest, manifest_for_url, EngineManifest, ENGINES, ORACLE};
