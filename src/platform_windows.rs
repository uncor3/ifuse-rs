use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use winfsp::host::{CoarseGuard, FileSystemHost, VolumeParams};

use crate::{
    DeviceTarget, Error, IfuseBuilder, MountHandle, Result, backend::Backend,
    windows_filesystem::IfuseFilesystem,
};

type Host = FileSystemHost<IfuseFilesystem, CoarseGuard>;

pub(crate) struct PlatformMount {
    mount_point: PathBuf,
    target: DeviceTarget,
    host: Mutex<Option<Host>>,
    backend: Mutex<Option<Backend>>,
}

impl PlatformMount {
    pub(crate) fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    pub(crate) fn target(&self) -> &DeviceTarget {
        &self.target
    }

    pub(crate) fn is_mounted(&self) -> bool {
        self.host
            .lock()
            .expect("WinFsp host lock poisoned")
            .is_some()
    }

    pub(crate) fn unmount(&self) -> Result<()> {
        if let Some(mut host) = self.host.lock().expect("WinFsp host lock poisoned").take() {
            host.stop();
            host.unmount();
        }
        if let Some(backend) = self.backend.lock().expect("backend lock poisoned").take() {
            backend.shutdown();
        }
        Ok(())
    }
}

impl Drop for PlatformMount {
    fn drop(&mut self) {
        if let Ok(host) = self.host.get_mut()
            && let Some(mut host) = host.take()
        {
            host.stop();
            host.unmount();
        }
        if let Ok(backend) = self.backend.get_mut()
            && let Some(backend) = backend.take()
        {
            backend.shutdown();
        }
    }
}

pub(crate) fn mount(builder: IfuseBuilder) -> Result<MountHandle> {
    let mount_point = builder.mount_point.clone().expect("validated mountpoint");
    let backend = Backend::start(builder.target.clone(), builder.source)?;
    let device_info = backend.device_info()?;
    let label = builder.volume_label.unwrap_or_else(|| {
        if device_info.model.is_empty() {
            "iPhone".into()
        } else {
            device_info.model.clone()
        }
    });
    let filesystem = IfuseFilesystem::new(backend.sender(), label);
    let mut params = VolumeParams::new();
    params
        .filesystem_name("ifuse-rs")
        .sector_size(4096)
        .sectors_per_allocation_unit(1)
        .max_component_length(255)
        .case_sensitive_search(true)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .reparse_points(true)
        .no_reparse_points_dir_check(true)
        .persistent_acls(false);

    let mut host = FileSystemHost::<_, CoarseGuard>::new(params, filesystem)
        .map_err(|error| Error::WinFsp(format!("create filesystem: {error}")))?;
    host.mount(&mount_point)
        .map_err(|error| Error::WinFsp(format!("mount at {}: {error}", mount_point.display())))?;
    if let Err(error) = host.start() {
        host.unmount();
        return Err(Error::WinFsp(format!("start dispatcher: {error}")));
    }

    Ok(MountHandle::new(PlatformMount {
        mount_point,
        target: builder.target,
        host: Mutex::new(Some(host)),
        backend: Mutex::new(Some(backend)),
    }))
}
