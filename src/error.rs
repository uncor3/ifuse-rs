use std::io;

use idevice::IdeviceError;
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("a mountpoint is required")]
    MountpointRequired,
    #[error("mountpoint does not exist: {0}")]
    MountpointMissing(std::path::PathBuf),
    #[error("mountpoint is not a directory: {0}")]
    MountpointNotDirectory(std::path::PathBuf),
    #[error("mountpoint must not already exist on Windows: {0}")]
    MountpointExists(std::path::PathBuf),
    #[error("UDID must not be empty")]
    EmptyUdid,
    #[error("application bundle identifier must not be empty")]
    EmptyBundleId,
    #[error("network pairing file path must not be empty")]
    EmptyPairingFile,
    #[error("volume label must not be empty")]
    EmptyVolumeLabel,
    #[error("invalid network address: {0}")]
    InvalidNetworkAddress(String),
    #[error("FUSE mount options are only supported on Linux")]
    WindowsMountOptions,
    #[error("invalid mount option: {0}")]
    InvalidMountOption(String),
    #[error("no matching iOS device found")]
    DeviceNotFound,
    #[error("device communication failed: {0}")]
    Device(#[from] IdeviceError),
    #[error("FUSE operation failed: {0}")]
    Fuse(io::Error),
    #[error("failed to start worker thread: {0}")]
    ThreadSpawn(io::Error),
    #[error("the device backend stopped unexpectedly")]
    BackendStopped,
    #[error("the mount is already unmounted")]
    AlreadyUnmounted,
    #[error("AFC returned an invalid response: {0}")]
    InvalidAfcResponse(String),
    #[error("path is not valid UTF-8: {0:?}")]
    NonUtf8Path(std::path::PathBuf),
    #[cfg(target_os = "windows")]
    #[error("WinFsp operation failed: {0}")]
    WinFsp(String),
}
