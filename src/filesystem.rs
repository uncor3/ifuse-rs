use std::ffi::OsStr;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, LockOwner, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
    WriteFlags,
};
use idevice::IdeviceError;
use idevice::afc::errors::AfcError;
use idevice::afc::opcode::{AfcFopenMode, LinkType};

use crate::BackendSender;
use crate::afc::{RemoteDeviceInfo, RemoteFileInfo, RemoteFileType};
use crate::backend::{BackendCommand, request};
use crate::inode::{InodeTable, child, parent};
use crate::{Error, Result};

const TTL: Duration = Duration::from_secs(1);

pub(crate) struct IfuseFilesystem {
    backend: BackendSender,
    inodes: Mutex<InodeTable>,
    device_info: RemoteDeviceInfo,
    uid: u32,
    gid: u32,
}

impl IfuseFilesystem {
    pub fn new(backend: BackendSender, device_info: RemoteDeviceInfo) -> Self {
        Self {
            backend,
            inodes: Mutex::new(InodeTable::default()),
            device_info,
            // SAFETY: these libc calls have no preconditions.
            uid: unsafe { libc::geteuid() },
            // SAFETY: these libc calls have no preconditions.
            gid: unsafe { libc::getegid() },
        }
    }

    fn path(&self, inode: INodeNo) -> Result<String> {
        self.inodes
            .lock()
            .expect("inode lock poisoned")
            .path(inode)
            .ok_or_else(|| Error::InvalidAfcResponse(format!("unknown inode {inode}")))
    }

    fn named_path(&self, parent_inode: INodeNo, name: &OsStr) -> Result<String> {
        let name = name
            .to_str()
            .ok_or_else(|| Error::NonUtf8Path(Path::new(name).to_path_buf()))?;
        Ok(child(&self.path(parent_inode)?, name))
    }

    fn info(&self, path: String) -> Result<RemoteFileInfo> {
        request(&self.backend, |reply| BackendCommand::FileInfo(path, reply))
    }

    fn attr(&self, inode: INodeNo, info: &RemoteFileInfo) -> FileAttr {
        let kind = file_type(info.kind);
        let perm = match kind {
            FileType::Directory => 0o755,
            FileType::Symlink => 0o777,
            _ => 0o644,
        };
        FileAttr {
            ino: inode,
            size: info.size,
            blocks: info.blocks,
            atime: info.modified,
            mtime: info.modified,
            ctime: info.modified,
            crtime: info.created,
            kind,
            perm,
            nlink: info.nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: self.device_info.block_size,
            flags: 0,
        }
    }

    fn entry(&self, path: &str, info: &RemoteFileInfo) -> (INodeNo, FileAttr) {
        let inode = self.inodes.lock().expect("inode lock poisoned").inode(path);
        (inode, self.attr(inode, info))
    }

    fn open_path(&self, path: String, flags: i32) -> Result<u64> {
        let mode = open_mode(flags)?;
        request(&self.backend, |reply| {
            BackendCommand::Open(path, mode, reply)
        })
    }
}

impl Filesystem for IfuseFilesystem {
    fn lookup(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let result = self.named_path(parent, name).and_then(|path| {
            let info = self.info(path.clone())?;
            Ok(self.entry(&path, &info).1)
        });
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn getattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        _handle: Option<FileHandle>,
        reply: ReplyAttr,
    ) {
        let result = self.path(inode).and_then(|path| self.info(path));
        match result {
            Ok(info) => reply.attr(&TTL, &self.attr(inode, &info)),
            Err(error) => reply.error(errno(&error)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _request: &Request,
        inode: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _handle: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let result = self.path(inode).and_then(|path| {
            if let Some(size) = size {
                request(&self.backend, |reply| {
                    BackendCommand::Truncate(path.clone(), size, reply)
                })?;
            }
            if let Some(mtime) = mtime {
                let mtime = match mtime {
                    TimeOrNow::SpecificTime(time) => time,
                    TimeOrNow::Now => SystemTime::now(),
                };
                request(&self.backend, |reply| {
                    BackendCommand::SetMtime(path.clone(), mtime, reply)
                })?;
            }
            self.info(path)
        });
        match result {
            Ok(info) => reply.attr(&TTL, &self.attr(inode, &info)),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn readlink(&self, _request: &Request, inode: INodeNo, reply: ReplyData) {
        let result = self
            .path(inode)
            .and_then(|path| self.info(path))
            .and_then(|info| {
                info.link_target
                    .ok_or_else(|| Error::InvalidAfcResponse("symlink has no target".into()))
            });
        match result {
            Ok(target) => reply.data(target.as_bytes()),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn mkdir(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let result = self.named_path(parent, name).and_then(|path| {
            request(&self.backend, |reply| {
                BackendCommand::Mkdir(path.clone(), reply)
            })?;
            let info = self.info(path.clone())?;
            Ok(self.entry(&path, &info).1)
        });
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn unlink(&self, request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove(request, parent, name, reply);
    }

    fn rmdir(&self, request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        self.remove(request, parent, name, reply);
    }

    fn symlink(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let result = self.named_path(parent, name).and_then(|path| {
            let target = target
                .to_str()
                .ok_or_else(|| Error::NonUtf8Path(target.to_path_buf()))?;
            request(&self.backend, |reply| {
                BackendCommand::Link(target.into(), path.clone(), LinkType::Symlink, reply)
            })?;
            let info = self.info(path.clone())?;
            Ok(self.entry(&path, &info).1)
        });
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn rename(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        new_parent: INodeNo,
        new_name: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if !flags.is_empty() {
            reply.error(Errno::EINVAL);
            return;
        }
        let result = self.named_path(parent, name).and_then(|from| {
            let to = self.named_path(new_parent, new_name)?;
            request(&self.backend, |reply| {
                BackendCommand::Rename(from.clone(), to.clone(), reply)
            })?;
            self.inodes
                .lock()
                .expect("inode lock poisoned")
                .rename(&from, &to);
            Ok(())
        });
        finish_empty(reply, result);
    }

    fn link(
        &self,
        _request: &Request,
        inode: INodeNo,
        new_parent: INodeNo,
        new_name: &OsStr,
        reply: ReplyEntry,
    ) {
        let result = self.path(inode).and_then(|target| {
            let link = self.named_path(new_parent, new_name)?;
            request(&self.backend, |reply| {
                BackendCommand::Link(target, link.clone(), LinkType::Hardlink, reply)
            })?;
            let info = self.info(link.clone())?;
            Ok(self.entry(&link, &info).1)
        });
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(0)),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn open(&self, _request: &Request, inode: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let result = self
            .path(inode)
            .and_then(|path| self.open_path(path, flags.0));
        match result {
            Ok(handle) => reply.opened(FileHandle(handle), FopenFlags::empty()),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn read(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let result = request(&self.backend, |reply| {
            BackendCommand::Read(handle.0, offset, size, reply)
        });
        match result {
            Ok(data) => reply.data(&data),
            Err(error) => reply.error(errno(&error)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let result = request(&self.backend, |reply| {
            BackendCommand::Write(handle.0, offset, data.to_vec(), reply)
        });
        match result {
            Ok(count) => reply.written(count),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn flush(
        &self,
        _request: &Request,
        _inode: INodeNo,
        _handle: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        _request: &Request,
        _inode: INodeNo,
        handle: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let result = request(&self.backend, |reply| {
            BackendCommand::Close(handle.0, reply)
        });
        finish_empty(reply, result);
    }

    fn fsync(
        &self,
        _request: &Request,
        _inode: INodeNo,
        _handle: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn readdir(
        &self,
        _request: &Request,
        inode: INodeNo,
        _handle: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let result = self.path(inode).and_then(|path| {
            let mut names = request(&self.backend, |reply| {
                BackendCommand::ListDir(path.clone(), reply)
            })?;
            if !names.iter().any(|name| name == ".") {
                names.insert(0, ".".into());
            }
            if !names.iter().any(|name| name == "..") {
                names.insert(1, "..".into());
            }
            for (index, name) in names.into_iter().enumerate().skip(offset as usize) {
                let entry_path = match name.as_str() {
                    "." => path.clone(),
                    ".." => parent(&path).to_owned(),
                    _ => child(&path, &name),
                };
                let info = self.info(entry_path.clone())?;
                let entry_inode = self
                    .inodes
                    .lock()
                    .expect("inode lock poisoned")
                    .inode(&entry_path);
                if reply.add(entry_inode, index as u64 + 1, file_type(info.kind), name) {
                    break;
                }
            }
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(errno(&error)),
        }
    }

    fn statfs(&self, _request: &Request, _inode: INodeNo, reply: ReplyStatfs) {
        let block_size = u64::from(self.device_info.block_size.max(1));
        reply.statfs(
            self.device_info.total_bytes / block_size,
            self.device_info.free_bytes / block_size,
            self.device_info.free_bytes / block_size,
            1_000_000_000,
            1_000_000_000,
            self.device_info.block_size,
            255,
            self.device_info.block_size,
        );
    }

    fn create(
        &self,
        _request: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let result = self.named_path(parent, name).and_then(|path| {
            let handle = self.open_path(path.clone(), flags | libc::O_CREAT)?;
            match self.info(path.clone()) {
                Ok(info) => {
                    let attr = self.entry(&path, &info).1;
                    Ok((attr, handle))
                }
                Err(error) => {
                    let _: Result<()> =
                        request(&self.backend, |reply| BackendCommand::Close(handle, reply));
                    Err(error)
                }
            }
        });
        match result {
            Ok((attr, handle)) => reply.created(
                &TTL,
                &attr,
                Generation(0),
                FileHandle(handle),
                FopenFlags::empty(),
            ),
            Err(error) => reply.error(errno(&error)),
        }
    }
}

impl IfuseFilesystem {
    fn remove(&self, _request: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let result = self.named_path(parent, name).and_then(|path| {
            request(&self.backend, |reply| {
                BackendCommand::Remove(path.clone(), reply)
            })?;
            self.inodes
                .lock()
                .expect("inode lock poisoned")
                .remove(&path);
            Ok(())
        });
        finish_empty(reply, result);
    }
}

fn finish_empty(reply: ReplyEmpty, result: Result<()>) {
    match result {
        Ok(()) => reply.ok(),
        Err(error) => reply.error(errno(&error)),
    }
}

fn file_type(kind: RemoteFileType) -> FileType {
    match kind {
        RemoteFileType::RegularFile => FileType::RegularFile,
        RemoteFileType::Directory => FileType::Directory,
        RemoteFileType::Symlink => FileType::Symlink,
        RemoteFileType::BlockDevice => FileType::BlockDevice,
        RemoteFileType::CharDevice => FileType::CharDevice,
        RemoteFileType::NamedPipe => FileType::NamedPipe,
        RemoteFileType::Socket => FileType::Socket,
    }
}

fn open_mode(flags: i32) -> Result<AfcFopenMode> {
    let append = flags & libc::O_APPEND != 0;
    let truncate = flags & libc::O_TRUNC != 0;
    match OpenFlags(flags).acc_mode() {
        OpenAccMode::O_RDONLY => Ok(AfcFopenMode::RdOnly),
        OpenAccMode::O_WRONLY if truncate => Ok(AfcFopenMode::WrOnly),
        OpenAccMode::O_WRONLY if append => Ok(AfcFopenMode::Append),
        OpenAccMode::O_WRONLY => Ok(AfcFopenMode::Rw),
        OpenAccMode::O_RDWR if truncate => Ok(AfcFopenMode::Wr),
        OpenAccMode::O_RDWR if append => Ok(AfcFopenMode::RdAppend),
        OpenAccMode::O_RDWR => Ok(AfcFopenMode::Rw),
    }
}

pub(crate) fn errno(error: &Error) -> Errno {
    let code = match error {
        Error::Device(IdeviceError::Afc(error)) => match error {
            AfcError::NoResources => libc::EMFILE,
            AfcError::ReadError => libc::ENOTDIR,
            AfcError::InvalidArg => libc::EINVAL,
            AfcError::ObjectNotFound => libc::ENOENT,
            AfcError::ObjectIsDir => libc::EISDIR,
            AfcError::DirNotEmpty => libc::ENOTEMPTY,
            AfcError::PermDenied => libc::EPERM,
            AfcError::ServiceNotConnected => libc::ENXIO,
            AfcError::OpTimeout => libc::ETIMEDOUT,
            AfcError::TooMuchData => libc::EFBIG,
            AfcError::EndOfData => libc::ENODATA,
            AfcError::OpNotSupported => libc::ENOSYS,
            AfcError::ObjectExists => libc::EEXIST,
            AfcError::ObjectBusy => libc::EBUSY,
            AfcError::NoSpaceLeft => libc::ENOSPC,
            AfcError::OpWouldBlock => libc::EWOULDBLOCK,
            AfcError::OpInterrupted => libc::EINTR,
            AfcError::OpInProgress => libc::EALREADY,
            _ => libc::EIO,
        },
        Error::NonUtf8Path(_) => libc::EILSEQ,
        Error::BackendStopped | Error::DeviceNotFound => libc::ENXIO,
        _ => libc::EIO,
    };
    Errno::from_i32(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_open_flags() {
        assert!(matches!(
            open_mode(libc::O_RDONLY),
            Ok(AfcFopenMode::RdOnly)
        ));
        assert!(matches!(
            open_mode(libc::O_WRONLY | libc::O_TRUNC),
            Ok(AfcFopenMode::WrOnly)
        ));
        assert!(matches!(
            open_mode(libc::O_RDWR | libc::O_APPEND),
            Ok(AfcFopenMode::RdAppend)
        ));
    }
}
