use std::{
    path::{Path, PathBuf},
    sync::Mutex,
};

use fuser::{BackgroundSession, Session};

use crate::{
    DeviceTarget, IfuseBuilder, MountHandle, Result, backend::Backend, filesystem::IfuseFilesystem,
};

pub(crate) struct PlatformMount {
    mount_point: PathBuf,
    target: DeviceTarget,
    session: Mutex<Option<BackgroundSession>>,
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
        self.session
            .lock()
            .expect("FUSE session lock poisoned")
            .as_ref()
            .is_some_and(|session| !session.guard.is_finished())
    }

    pub(crate) fn unmount(&self) -> Result<()> {
        let session = self
            .session
            .lock()
            .expect("FUSE session lock poisoned")
            .take();
        if let Some(session) = session {
            session.umount_and_join().map_err(crate::Error::Fuse)?;
        }
        self.stop_backend();
        Ok(())
    }

    fn stop_backend(&self) {
        if let Some(backend) = self.backend.lock().expect("backend lock poisoned").take() {
            backend.shutdown();
        }
    }
}

impl Drop for PlatformMount {
    fn drop(&mut self) {
        if let Ok(session) = self.session.get_mut()
            && let Some(session) = session.take()
        {
            let _ = session.umount_and_join();
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
    let config = builder.fuser_config()?;
    let backend = Backend::start(builder.target.clone(), builder.source)?;
    let filesystem = IfuseFilesystem::new(backend.sender(), backend.device_info()?);
    let session = Session::new(filesystem, &mount_point, &config)
        .and_then(Session::spawn)
        .map_err(crate::Error::Fuse)?;

    Ok(MountHandle::new(PlatformMount {
        mount_point,
        target: builder.target,
        session: Mutex::new(Some(session)),
        backend: Mutex::new(Some(backend)),
    }))
}
