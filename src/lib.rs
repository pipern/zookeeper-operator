#![allow(unused_imports)]

use snafu::{Backtrace, OptionExt, ResultExt, Snafu};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("ZooKeeperCluster has bad info: {}", info))]
    ZooKeeperClusterIsBad { info: String, backtrace: Backtrace },

    #[snafu(display("Failed to patch ZooKeeperCluster: {}", source))]
    ZooKeeperClusterPatchFailed {
        source: kube::Error,
        backtrace: Backtrace,
    },

    SerializationFailed {
        source: serde_json::Error,
        backtrace: Backtrace,
    },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// State machinery for kube, as exposeable to actix
pub mod manager;

pub use manager::Manager;
