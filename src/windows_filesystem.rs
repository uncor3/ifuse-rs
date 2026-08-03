use std::{
    ffi::c_void,
    os::windows::ffi::OsStringExt,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use idevice::{
    IdeviceError,
    afc::errors::AfcError,
    afc::opcode::{AfcFopenMode, LinkType},
};
use widestring::U16CStr;
use windows::Win32::Foundation::{
    STATUS_ACCESS_DENIED, STATUS_DEVICE_NOT_CONNECTED, STATUS_DIRECTORY_NOT_EMPTY,
    STATUS_DISK_FULL, STATUS_FILE_IS_A_DIRECTORY, STATUS_FILE_NOT_AVAILABLE,
    STATUS_INVALID_PARAMETER, STATUS_IO_DEVICE_ERROR, STATUS_NOT_A_DIRECTORY, STATUS_NOT_FOUND,
    STATUS_NOT_SUPPORTED, STATUS_OBJECT_NAME_COLLISION,
};
use winfsp::{
    FspError,
    filesystem::{
        DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo,
        WideNameInfo,
    },
};

use crate::{
    BackendSender, Error,
    afc::{RemoteDeviceInfo, RemoteFileInfo, RemoteFileType},
    backend::{BackendCommand, request},
};

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
const FILE_DIRECTORY_FILE: u32 = 0x1;
const FILE_WRITE_DATA: u32 = 0x2;
const FILE_APPEND_DATA: u32 = 0x4;
const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000000C;

#[derive(Debug)]
pub(crate) struct IfuseFileContext {
    path: String,
    descriptor: Option<u64>,
    directory: bool,
    delete_pending: AtomicBool,
    directory_entries: Mutex<Option<Vec<DirectoryEntry>>>,
}

#[derive(Debug)]
struct DirectoryEntry {
    name: String,
    info: RemoteFileInfo,
}

pub(crate) struct IfuseFilesystem {
    backend: BackendSender,
    volume_label: Mutex<String>,
}

impl IfuseFilesystem {
    pub(crate) fn new(backend: BackendSender, volume_label: String) -> Self {
        Self {
            backend,
            volume_label: Mutex::new(volume_label),
        }
    }

    fn call<T>(
        &self,
        make: impl FnOnce(std::sync::mpsc::SyncSender<crate::Result<T>>) -> BackendCommand,
    ) -> winfsp::Result<T> {
        request(&self.backend, make).map_err(map_backend_error)
    }

    fn stat(&self, path: &str) -> winfsp::Result<RemoteFileInfo> {
        self.call(|reply| BackendCommand::FileInfo(path.into(), reply))
    }

    fn fill_info(&self, path: &str, output: &mut FileInfo) -> winfsp::Result<RemoteFileInfo> {
        let info = self.stat(path)?;
        *output = file_info(&info);
        Ok(info)
    }

    fn create_context(
        &self,
        path: String,
        directory: bool,
        descriptor: Option<u64>,
    ) -> IfuseFileContext {
        IfuseFileContext {
            path,
            descriptor,
            directory,
            delete_pending: AtomicBool::new(false),
            directory_entries: Mutex::new(None),
        }
    }
}

impl FileSystemContext for IfuseFilesystem {
    type FileContext = IfuseFileContext;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        _security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> winfsp::Result<FileSecurity> {
        let info = self.stat(&afc_path(file_name))?;
        Ok(FileSecurity {
            reparse: info.kind == RemoteFileType::Symlink,
            sz_security_descriptor: 0,
            attributes: file_attributes(&info),
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        granted_access: winfsp_sys::FILE_ACCESS_RIGHTS,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = afc_path(file_name);
        let info = self.fill_info(&path, file_info.as_mut())?;
        let directory = info.kind == RemoteFileType::Directory;
        let descriptor = if directory {
            None
        } else {
            Some(self.call(|reply| {
                BackendCommand::Open(path.clone(), access_mode(granted_access as u32), reply)
            })?)
        };
        Ok(self.create_context(path, directory, descriptor))
    }

    fn close(&self, context: Self::FileContext) {
        if let Some(descriptor) = context.descriptor {
            if let Err(error) = self.call(|reply| BackendCommand::Close(descriptor, reply)) {
                tracing::warn!(path = %context.path, %error, "failed to close AFC descriptor");
            }
        }
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: winfsp_sys::FILE_ACCESS_RIGHTS,
        _file_attributes: winfsp_sys::FILE_FLAGS_AND_ATTRIBUTES,
        _security_descriptor: Option<&[c_void]>,
        allocation_size: u64,
        extra_buffer: Option<&[u8]>,
        extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> winfsp::Result<Self::FileContext> {
        let path = afc_path(file_name);
        if extra_buffer_is_reparse_point {
            let target = parse_symlink_reparse(extra_buffer.unwrap_or_default())?;
            self.call(|reply| {
                BackendCommand::Link(target, path.clone(), LinkType::Symlink, reply)
            })?;
            self.fill_info(&path, file_info.as_mut())?;
            return Ok(self.create_context(path, false, None));
        }
        if create_options & FILE_DIRECTORY_FILE != 0 {
            self.call(|reply| BackendCommand::Mkdir(path.clone(), reply))?;
            self.fill_info(&path, file_info.as_mut())?;
            return Ok(self.create_context(path, true, None));
        }

        let descriptor =
            self.call(|reply| BackendCommand::Open(path.clone(), AfcFopenMode::Wr, reply))?;
        if allocation_size != 0 {
            self.call(|reply| BackendCommand::TruncateHandle(descriptor, allocation_size, reply))?;
        }
        self.fill_info(&path, file_info.as_mut())?;
        Ok(self.create_context(path, false, Some(descriptor)))
    }

    fn cleanup(&self, context: &Self::FileContext, _file_name: Option<&U16CStr>, _flags: u32) {
        if context.delete_pending.load(Ordering::Acquire) {
            if let Err(error) =
                self.call(|reply| BackendCommand::Remove(context.path.clone(), reply))
            {
                tracing::warn!(path = %context.path, %error, "failed to delete AFC path");
            }
        }
    }

    fn flush(
        &self,
        context: Option<&Self::FileContext>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        if let Some(context) = context {
            self.fill_info(&context.path, file_info)?;
        }
        Ok(())
    }

    fn get_file_info(
        &self,
        context: &Self::FileContext,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        self.fill_info(&context.path, file_info)?;
        Ok(())
    }

    fn overwrite(
        &self,
        context: &Self::FileContext,
        _file_attributes: winfsp_sys::FILE_FLAGS_AND_ATTRIBUTES,
        _replace_file_attributes: bool,
        allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let descriptor = descriptor(context)?;
        self.call(|reply| BackendCommand::TruncateHandle(descriptor, allocation_size, reply))?;
        self.fill_info(&context.path, file_info)?;
        Ok(())
    }

    fn read_directory(
        &self,
        context: &Self::FileContext,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> winfsp::Result<u32> {
        if !context.directory {
            return Err(STATUS_NOT_A_DIRECTORY.into());
        }
        if marker.is_none() {
            let names = self.call(|reply| BackendCommand::ListDir(context.path.clone(), reply))?;
            let mut entries = Vec::with_capacity(names.len());
            for name in names.into_iter().filter(|name| name != "." && name != "..") {
                let child = join_afc_path(&context.path, &name);
                entries.push(DirectoryEntry {
                    name,
                    info: self.stat(&child)?,
                });
            }
            entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
            *context.directory_entries.lock().unwrap() = Some(entries);
        }

        let marker = marker.inner().map(String::from_utf16_lossy);
        let entries = context.directory_entries.lock().unwrap();
        let entries = entries.as_deref().unwrap_or_default();
        let start = directory_start(entries, marker.as_deref());
        let mut cursor = 0;
        let mut reached_end = true;
        for entry in &entries[start..] {
            let mut output: DirInfo<255> = DirInfo::new();
            *output.file_info_mut() = file_info(&entry.info);
            output.set_name(&entry.name)?;
            if !output.append_to_buffer(buffer, &mut cursor) {
                reached_end = false;
                break;
            }
        }
        if reached_end {
            DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
        }
        Ok(cursor)
    }

    fn rename(
        &self,
        _context: &Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> winfsp::Result<()> {
        let from = afc_path(file_name);
        let to = afc_path(new_file_name);
        if replace_if_exists && self.stat(&to).is_ok() {
            self.call(|reply| BackendCommand::Remove(to.clone(), reply))?;
        }
        self.call(|reply| BackendCommand::Rename(from, to, reply))
    }

    fn set_basic_info(
        &self,
        context: &Self::FileContext,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        if last_write_time != 0 {
            let time =
                UNIX_EPOCH + Duration::from_nanos(windows_time_to_unix_nanos(last_write_time));
            self.call(|reply| BackendCommand::SetMtime(context.path.clone(), time, reply))?;
        }
        self.fill_info(&context.path, file_info)?;
        Ok(())
    }

    fn set_delete(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        delete_file: bool,
    ) -> winfsp::Result<()> {
        if delete_file && context.directory {
            let children =
                self.call(|reply| BackendCommand::ListDir(context.path.clone(), reply))?;
            if children.iter().any(|name| name != "." && name != "..") {
                return Err(STATUS_DIRECTORY_NOT_EMPTY.into());
            }
        }
        context.delete_pending.store(delete_file, Ordering::Release);
        Ok(())
    }

    fn set_file_size(
        &self,
        context: &Self::FileContext,
        new_size: u64,
        _set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<()> {
        let descriptor = descriptor(context)?;
        self.call(|reply| BackendCommand::TruncateHandle(descriptor, new_size, reply))?;
        self.fill_info(&context.path, file_info)?;
        Ok(())
    }

    fn read(
        &self,
        context: &Self::FileContext,
        buffer: &mut [u8],
        offset: u64,
    ) -> winfsp::Result<u32> {
        let descriptor = descriptor(context)?;
        let bytes = self.call(|reply| {
            BackendCommand::Read(
                descriptor,
                offset,
                buffer.len().min(u32::MAX as usize) as u32,
                reply,
            )
        })?;
        buffer[..bytes.len()].copy_from_slice(&bytes);
        Ok(bytes.len() as u32)
    }

    fn write(
        &self,
        context: &Self::FileContext,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        _constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> winfsp::Result<u32> {
        let descriptor = descriptor(context)?;
        let offset = if write_to_eof {
            self.stat(&context.path)?.size
        } else {
            offset
        };
        let count =
            self.call(|reply| BackendCommand::Write(descriptor, offset, buffer.to_vec(), reply))?;
        self.fill_info(&context.path, file_info)?;
        Ok(count)
    }

    fn get_volume_info(&self, output: &mut VolumeInfo) -> winfsp::Result<()> {
        let info: RemoteDeviceInfo = self.call(BackendCommand::DeviceInfo)?;
        output.total_size = info.total_bytes;
        output.free_size = info.free_bytes;
        output.set_volume_label(self.volume_label.lock().unwrap().as_str());
        Ok(())
    }

    fn set_volume_label(
        &self,
        volume_label: &U16CStr,
        volume_info: &mut VolumeInfo,
    ) -> winfsp::Result<()> {
        let label = wide_string(volume_label);
        *self.volume_label.lock().unwrap() = label.clone();
        self.get_volume_info(volume_info)?;
        volume_info.set_volume_label(label);
        Ok(())
    }

    fn get_reparse_point_by_name(
        &self,
        file_name: &U16CStr,
        _is_directory: bool,
        buffer: &mut [u8],
    ) -> winfsp::Result<u64> {
        let target = self
            .stat(&afc_path(file_name))?
            .link_target
            .ok_or_else(|| FspError::NTSTATUS(STATUS_NOT_SUPPORTED.0))?;
        write_symlink_reparse(&target, buffer)
    }

    fn get_reparse_point(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        buffer: &mut [u8],
    ) -> winfsp::Result<u64> {
        let target = self
            .stat(&context.path)?
            .link_target
            .ok_or_else(|| FspError::NTSTATUS(STATUS_NOT_SUPPORTED.0))?;
        write_symlink_reparse(&target, buffer)
    }

    fn set_reparse_point(
        &self,
        context: &Self::FileContext,
        _file_name: &U16CStr,
        buffer: &[u8],
    ) -> winfsp::Result<()> {
        let target = parse_symlink_reparse(buffer)?;
        self.call(|reply| BackendCommand::Remove(context.path.clone(), reply))?;
        self.call(|reply| {
            BackendCommand::Link(target, context.path.clone(), LinkType::Symlink, reply)
        })
    }
}

fn descriptor(context: &IfuseFileContext) -> winfsp::Result<u64> {
    context
        .descriptor
        .ok_or_else(|| FspError::NTSTATUS(STATUS_FILE_IS_A_DIRECTORY.0))
}

fn afc_path(path: &U16CStr) -> String {
    let value = wide_string(path).replace('\\', "/");
    if value.is_empty() || value == "/" {
        "/".into()
    } else if value.starts_with('/') {
        value
    } else {
        format!("/{value}")
    }
}

fn wide_string(value: &U16CStr) -> String {
    std::ffi::OsString::from_wide(value.as_slice())
        .to_string_lossy()
        .into_owned()
}

fn join_afc_path(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

fn directory_start(entries: &[DirectoryEntry], marker: Option<&str>) -> usize {
    marker.map_or(0, |marker| {
        entries.partition_point(|entry| entry.name.as_str() <= marker)
    })
}

fn access_mode(access: u32) -> AfcFopenMode {
    if access & FILE_APPEND_DATA != 0 {
        AfcFopenMode::RdAppend
    } else if access & FILE_WRITE_DATA != 0 {
        AfcFopenMode::Rw
    } else {
        AfcFopenMode::RdOnly
    }
}

fn file_attributes(info: &RemoteFileInfo) -> u32 {
    match info.kind {
        RemoteFileType::Directory => FILE_ATTRIBUTE_DIRECTORY,
        RemoteFileType::Symlink => FILE_ATTRIBUTE_REPARSE_POINT,
        _ => FILE_ATTRIBUTE_NORMAL,
    }
}

fn file_info(info: &RemoteFileInfo) -> FileInfo {
    let creation = system_time_to_windows_time(info.created);
    let modified = system_time_to_windows_time(info.modified);
    FileInfo {
        file_attributes: file_attributes(info),
        reparse_tag: (info.kind == RemoteFileType::Symlink)
            .then_some(IO_REPARSE_TAG_SYMLINK)
            .unwrap_or(0),
        allocation_size: info.blocks.saturating_mul(4096).max(info.size),
        file_size: info.size,
        creation_time: creation,
        last_access_time: modified,
        last_write_time: modified,
        change_time: modified,
        ..Default::default()
    }
}

const WINDOWS_EPOCH_TICKS: i128 = 116_444_736_000_000_000;

fn system_time_to_windows_time(time: SystemTime) -> u64 {
    let nanos = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i128::MAX as u128) as i128;
    (WINDOWS_EPOCH_TICKS + nanos / 100).max(0) as u64
}

fn windows_time_to_unix_nanos(ticks: u64) -> u64 {
    (i128::from(ticks).saturating_sub(WINDOWS_EPOCH_TICKS).max(0) * 100).min(u64::MAX as i128)
        as u64
}

fn map_backend_error(error: Error) -> FspError {
    let status = match error {
        Error::Device(IdeviceError::Afc(AfcError::ObjectNotFound))
        | Error::Device(IdeviceError::DeviceNotFound) => STATUS_NOT_FOUND,
        Error::Device(IdeviceError::Afc(AfcError::ObjectExists)) => STATUS_OBJECT_NAME_COLLISION,
        Error::Device(IdeviceError::Afc(AfcError::ObjectIsDir)) => STATUS_FILE_IS_A_DIRECTORY,
        Error::Device(IdeviceError::Afc(AfcError::PermDenied)) => STATUS_ACCESS_DENIED,
        Error::Device(IdeviceError::Afc(AfcError::NoSpaceLeft)) => STATUS_DISK_FULL,
        Error::Device(IdeviceError::Afc(AfcError::DirNotEmpty)) => STATUS_DIRECTORY_NOT_EMPTY,
        Error::Device(IdeviceError::Afc(AfcError::InvalidArg)) => STATUS_INVALID_PARAMETER,
        Error::Device(IdeviceError::Afc(AfcError::OpNotSupported)) => STATUS_NOT_SUPPORTED,
        Error::Device(IdeviceError::Afc(AfcError::ServiceNotConnected))
        | Error::Device(IdeviceError::NoEstablishedConnection)
        | Error::BackendStopped
        | Error::DeviceNotFound => STATUS_DEVICE_NOT_CONNECTED,
        Error::Device(IdeviceError::Timeout)
        | Error::Device(IdeviceError::Afc(AfcError::OpTimeout)) => STATUS_FILE_NOT_AVAILABLE,
        _ => STATUS_IO_DEVICE_ERROR,
    };
    FspError::NTSTATUS(status.0)
}

fn write_symlink_reparse(target: &str, buffer: &mut [u8]) -> winfsp::Result<u64> {
    let target = target.replace('/', "\\");
    let wide: Vec<u16> = target.encode_utf16().collect();
    let path_bytes = wide.len() * 2;
    let total = 20 + path_bytes * 2;
    if buffer.len() < total {
        return Err(FspError::NTSTATUS(STATUS_INVALID_PARAMETER.0));
    }
    buffer[..total].fill(0);
    buffer[0..4].copy_from_slice(&IO_REPARSE_TAG_SYMLINK.to_le_bytes());
    buffer[4..6].copy_from_slice(&((12 + path_bytes * 2) as u16).to_le_bytes());
    buffer[10..12].copy_from_slice(&(path_bytes as u16).to_le_bytes());
    buffer[12..14].copy_from_slice(&(path_bytes as u16).to_le_bytes());
    buffer[14..16].copy_from_slice(&(path_bytes as u16).to_le_bytes());
    buffer[16..20].copy_from_slice(&1u32.to_le_bytes());
    for (index, character) in wide.iter().chain(wide.iter()).enumerate() {
        let offset = 20 + index * 2;
        buffer[offset..offset + 2].copy_from_slice(&character.to_le_bytes());
    }
    Ok(total as u64)
}

fn parse_symlink_reparse(buffer: &[u8]) -> winfsp::Result<String> {
    if buffer.len() < 20
        || u32::from_le_bytes(buffer[0..4].try_into().unwrap()) != IO_REPARSE_TAG_SYMLINK
    {
        return Err(FspError::NTSTATUS(STATUS_NOT_SUPPORTED.0));
    }
    let offset = u16::from_le_bytes(buffer[12..14].try_into().unwrap()) as usize;
    let length = u16::from_le_bytes(buffer[14..16].try_into().unwrap()) as usize;
    let start = 20 + offset;
    let end = start.saturating_add(length);
    if end > buffer.len() || length % 2 != 0 {
        return Err(FspError::NTSTATUS(STATUS_INVALID_PARAMETER.0));
    }
    let wide = buffer[start..end]
        .chunks_exact(2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .collect::<Vec<_>>();
    Ok(String::from_utf16_lossy(&wide).replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_modes_match_windows_rights() {
        assert!(matches!(access_mode(0), AfcFopenMode::RdOnly));
        assert!(matches!(access_mode(FILE_WRITE_DATA), AfcFopenMode::Rw));
        assert!(matches!(
            access_mode(FILE_APPEND_DATA),
            AfcFopenMode::RdAppend
        ));
    }

    #[test]
    fn symlink_reparse_round_trip() {
        let mut data = vec![0; 1024];
        let size = write_symlink_reparse("../Documents/file", &mut data).unwrap() as usize;
        assert_eq!(
            parse_symlink_reparse(&data[..size]).unwrap(),
            "../Documents/file"
        );
    }
}
