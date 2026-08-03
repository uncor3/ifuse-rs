#![doc = "Reusable iOS AFC filesystem mounts for Linux and Windows."]

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
compile_error!("ifuse-rs only supports Linux and Windows");

mod afc;
mod backend;
mod error;
mod options;

#[cfg(target_os = "linux")]
mod filesystem;
#[cfg(target_os = "linux")]
mod inode;
#[cfg(target_os = "linux")]
mod platform_linux;
#[cfg(target_os = "linux")]
use platform_linux as platform;

#[cfg(target_os = "windows")]
mod platform_windows;
#[cfg(target_os = "windows")]
mod windows_filesystem;
#[cfg(target_os = "windows")]
use platform_windows as platform;

pub use error::{Error, Result};
pub use options::{AppInfo, DeviceTarget, IfuseBuilder, MountSource};

use std::{path::Path, sync::Arc};

use backend::BackendCommand;

/// An active mount. Clones refer to the same idempotently-unmounted mount.
#[derive(Clone)]
pub struct MountHandle {
    inner: Arc<platform::PlatformMount>,
}

impl std::fmt::Debug for MountHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MountHandle")
            .field("mount_point", &self.mount_point())
            .field("target", self.target())
            .field("is_mounted", &self.is_mounted())
            .finish()
    }
}

impl MountHandle {
    pub(crate) fn new(inner: platform::PlatformMount) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn mount_point(&self) -> &Path {
        self.inner.mount_point()
    }

    pub fn target(&self) -> &DeviceTarget {
        self.inner.target()
    }

    pub fn is_mounted(&self) -> bool {
        self.inner.is_mounted()
    }

    /// Cleanly stops the filesystem and removes the mount. Repeated calls are safe.
    pub async fn unmount(&self) -> Result<()> {
        self.inner.unmount()
    }
}

impl IfuseBuilder {
    /// Connects to the device and starts the platform filesystem dispatcher.
    pub async fn mount(self) -> Result<MountHandle> {
        self.validate()?;
        platform::mount(self)
    }
}

/// Lists installed applications with iTunes file sharing enabled.
pub async fn list_apps(target: DeviceTarget) -> Result<Vec<AppInfo>> {
    backend::list_apps_blocking(target)
}

pub(crate) type BackendSender = std::sync::mpsc::Sender<BackendCommand>;
