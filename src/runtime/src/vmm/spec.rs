//! InstanceSpec re-exported from `a3s-box-core`.
//!
//! The canonical definition lives in `a3s_box_core::vmm`. Re-exported here
//! so existing callers using `crate::vmm::InstanceSpec` continue to work.

pub use a3s_box_core::vmm::{
    Entrypoint, FsMount, InstanceSpec, NetworkInstanceConfig, TeeInstanceConfig,
};
