//! Library form of blobdev, so the volume and backend layers can be tested
//! independently of the ublk device and the CLI.

pub mod meta;
pub mod readcache;
pub mod s3;
pub mod store;
pub mod ublk_tgt;
pub mod volume;
pub mod webdav;
