use fuse_mt::{
    CallbackResult, CreatedEntry, FileAttr, FilesystemMT, RequestInfo, ResultCreate, ResultData,
    ResultEmpty, ResultEntry, ResultOpen, ResultReaddir, ResultSlice, ResultStatfs, ResultWrite,
    ResultXattr, Xattr,
};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::fmt::Debug;
use std::future::Future;
use std::os::unix::ffi::OsStringExt;
use std::path::Path;
use std::time::SystemTime;

use crate::common::{
    DirEntry, FsError, FsResult, Gid, Mode, NodeAttrs, NodeKind, NumBytes, OpenFlags, Statfs, Uid,
};
use crate::low_level_api::{AsyncFilesystem, FileHandle};
use cryfs_utils::{
    async_drop::{AsyncDrop, AsyncDropGuard},
    safe_panic,
};

// TODO Make sure each function checks the preconditions on its parameters, e.g. paths must be absolute
// TODO Check which of the logging statements parameters actually need :? formatting
// TODO Decide for logging whether we want parameters in parentheses or not, currently it's inconsistent
// TODO Go through fuse documentation and syscall manpages to check for behavior and possible error codes, make sure we handle all of them
// TODO We don't need the multithreading from fuse_mt, it's probably better to use fuser instead.
// TODO Which operations are supposed to follow symlinks, which ones aren't? Make sure we handle that correctly. Does fuse automatically deref symlinks before calling us?
// TODO https://www.cs.hmc.edu/~geoff/classes/hmc.cs135.201001/homework/fuse/fuse_doc.html#function-purposes :
//  - "Set flag_nullpath_ok nonzero if your code can accept a NULL path argument (because it gets file information from fi->fh) for the following operations: fgetattr, flush, fsync, fsyncdir, ftruncate, lock, read, readdir, release, releasedir, and write. This will allow FUSE to run more efficiently."
//  - Check function documentation and corner cases are as I expect them to be

pub struct BackendAdapter<Fs>
where
    Fs: AsyncFilesystem + AsyncDrop<Error = FsError> + Debug + Send + Sync + 'static,
{
    // TODO RwLock is only needed for async drop. Can we remove it?
    fs: tokio::sync::RwLock<AsyncDropGuard<Fs>>,

    runtime: tokio::runtime::Handle,
}

impl<Fs> Debug for BackendAdapter<Fs>
where
    Fs: AsyncFilesystem + AsyncDrop<Error = FsError> + Debug + Send + Sync + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackendAdapter")
            .field("fs", &self.fs)
            .finish()
    }
}

impl<Fs> BackendAdapter<Fs>
where
    Fs: AsyncFilesystem + AsyncDrop<Error = FsError> + Debug + Send + Sync + 'static,
{
    pub fn new(fs: AsyncDropGuard<Fs>, runtime: tokio::runtime::Handle) -> Self {
        Self {
            fs: tokio::sync::RwLock::new(fs),
            runtime,
        }
    }

    fn run_async<R, F>(&self, log_msg: &str, func: impl FnOnce() -> F) -> Result<R, libc::c_int>
    where
        F: Future<Output = FsResult<R>>,
    {
        // TODO Is it ok to call block_on concurrently for multiple fs operations? Probably not.
        self.runtime.block_on(async move {
            log::info!("{}...", log_msg);
            let result = func().await;
            match result {
                Ok(ok) => {
                    log::info!("{}...done", log_msg);
                    Ok(ok)
                }
                Err(err) => {
                    log::info!("{}...failed: {}", log_msg, err);
                    Err(err.system_error_code())
                }
            }
        })
    }
}

impl<Fs> FilesystemMT for BackendAdapter<Fs>
where
    Fs: AsyncFilesystem + AsyncDrop<Error = FsError> + Debug + Send + Sync + 'static,
{
    fn init(&self, req: RequestInfo) -> ResultEmpty {
        self.run_async(&format!("init"), move || async move {
            let fs = self.fs.read().await;
            fs.init(req.into()).await?;
            Ok(())
        })
    }

    fn destroy(&self) {
        self.run_async(&format!("destroy"), move || async move {
            let mut fs = self.fs.write().await;
            fs.destroy().await;
            fs.async_drop().await?;
            Ok(())
        })
        .expect("failed to drop file system");

        // TODO Is there a way to do the above without a call to expect()?
    }

    fn getattr(&self, req: RequestInfo, path: &Path, fh: Option<u64>) -> ResultEntry {
        self.run_async(&format!("getattr {path:?}"), move || async move {
            let response = self
                .fs
                .read()
                .await
                .getattr(req.into(), path, fh.into_fh())
                .await?;
            Ok((response.ttl, convert_node_attrs(response.attrs)))
        })
    }

    fn chmod(&self, req: RequestInfo, path: &Path, fh: Option<u64>, mode: u32) -> ResultEmpty {
        self.run_async(&format!("chmod({path:?}, mode={mode})"), || async move {
            self.fs
                .read()
                .await
                .chmod(req.into(), path, fh.into_fh(), Mode::from(mode))
                .await
        })
    }

    fn chown(
        &self,
        req: RequestInfo,
        path: &Path,
        fh: Option<u64>,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> ResultEmpty {
        self.run_async(
            &format!("chown({path:?}, uid={uid:?}, gid={gid:?})"),
            || async move {
                self.fs
                    .read()
                    .await
                    .chown(
                        req.into(),
                        path,
                        fh.into_fh(),
                        uid.into_uid(),
                        gid.into_gid(),
                    )
                    .await
            },
        )
    }

    fn truncate(&self, req: RequestInfo, path: &Path, fh: Option<u64>, size: u64) -> ResultEmpty {
        self.run_async(&format!("truncate({path:?}, {size})"), move || async move {
            self.fs
                .read()
                .await
                .truncate(req.into(), path, fh.into_fh(), NumBytes::from(size))
                .await
        })
    }

    fn utimens(
        &self,
        req: RequestInfo,
        path: &Path,
        fh: Option<u64>,
        atime: Option<SystemTime>,
        mtime: Option<SystemTime>,
    ) -> ResultEmpty {
        self.run_async(
            &format!("utimens({path:?}, fh={fh:?}, atime={atime:?}, mtime={mtime:?})"),
            || async move {
                self.fs
                    .read()
                    .await
                    .utimens(req.into(), path, fh.into_fh(), atime, mtime)
                    .await
            },
        )
    }

    /// Set timestamps of a filesystem entry (with extra options only used on MacOS).
    fn utimens_macos(
        &self,
        req: RequestInfo,
        path: &Path,
        fh: Option<u64>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<u32>,
    ) -> ResultEmpty {
        self.run_async(&format!("utimens({path:?}, fh={fh:?}, crtime={crtime:?}, chgtime={chgtime:?}, bkuptime={bkuptime:?}"), ||async move {
            self.fs.read().await.utimens_macos(req.into(), path, fh.into_fh(), crtime, chgtime, bkuptime, flags).await
        })
    }

    fn readlink(&self, req: RequestInfo, path: &Path) -> ResultData {
        self.run_async(&format!("readlink({path:?})"), move || async move {
            let path = self.fs.read().await.readlink(req.into(), path).await?;
            // TODO is OsStr the best way to convert our path to the return value?
            Ok(path.into_os_string().into_vec())
        })
    }

    fn mknod(
        &self,
        req: RequestInfo,
        parent: &Path,
        name: &OsStr,
        mode: u32,
        rdev: u32,
    ) -> ResultEntry {
        self.run_async(
            &format!("mknod({parent:?}, name={name:?}, mode={mode}, rdev={rdev})"),
            move || async move {
                let response = self
                    .fs
                    .read()
                    .await
                    .mknod(
                        req.into(),
                        parent,
                        &parse_node_name(name),
                        Mode::from(mode),
                        rdev,
                    )
                    .await?;
                Ok((response.ttl, convert_node_attrs(response.attrs)))
            },
        )
    }

    fn mkdir(&self, req: RequestInfo, parent: &Path, name: &OsStr, mode: u32) -> ResultEntry {
        self.run_async(
            &format!("mkdir({parent:?}, name={name:?}, mode={mode})"),
            move || async move {
                let response = self
                    .fs
                    .read()
                    .await
                    .mkdir(req.into(), parent, &parse_node_name(name), Mode::from(mode))
                    .await?;
                Ok((response.ttl, convert_node_attrs(response.attrs)))
            },
        )
    }

    fn unlink(&self, req: RequestInfo, parent: &Path, name: &OsStr) -> ResultEmpty {
        let name = &parse_node_name(name);
        self.run_async(
            &format!("unlink({parent:?}, name={name:?})"),
            move || async move { self.fs.read().await.unlink(req.into(), parent, &name).await },
        )
    }

    fn rmdir(&self, req: RequestInfo, parent: &Path, name: &OsStr) -> ResultEmpty {
        let name = &parse_node_name(name);
        self.run_async(
            &format!("rmdir({parent:?}, name={name:?})"),
            move || async move { self.fs.read().await.rmdir(req.into(), parent, &name).await },
        )
    }

    fn symlink(&self, req: RequestInfo, parent: &Path, name: &OsStr, target: &Path) -> ResultEntry {
        self.run_async(
            &format!("symlink({parent:?}, parent={parent:?} name={name:?}, target={target:?})"),
            move || async move {
                let response = self
                    .fs
                    .read()
                    .await
                    .symlink(req.into(), parent, &parse_node_name(name), target)
                    .await?;
                Ok((response.ttl, convert_node_attrs(response.attrs)))
            },
        )
    }

    fn rename(
        &self,
        req: RequestInfo,
        oldparent: &Path,
        oldname: &OsStr,
        newparent: &Path,
        newname: &OsStr,
    ) -> ResultEmpty {
        let oldname = &parse_node_name(oldname);
        let newname = &parse_node_name(newname);
        self.run_async(
            &format!(
                "rename(oldparent={oldparent:?}, oldname={oldname:?}, newparent={newparent:?}, newname={newname:?})"
            ),
            move || async move {
                self.fs.read().await.rename(
                    req.into(),
                    oldparent,
                    oldname,
                    newparent,
                    newname,
                ).await
            },
        )
    }

    fn link(
        &self,
        req: RequestInfo,
        path: &Path,
        newparent: &Path,
        newname: &OsStr,
    ) -> ResultEntry {
        self.run_async(
            &format!("link(path={path:?}, newparent={newparent:?}, newname={newname:?})"),
            move || async move {
                let response = self
                    .fs
                    .read()
                    .await
                    .link(req.into(), path, newparent, &parse_node_name(newname))
                    .await?;
                Ok((response.ttl, convert_node_attrs(response.attrs)))
            },
        )
    }

    fn open(&self, req: RequestInfo, path: &Path, flags: u32) -> ResultOpen {
        // TODO flags should be i32 and is in fuser, but fuse_mt accidentally converts it to u32. Undo that.
        let flags = flags as i32;
        self.run_async(
            &format!("open({path:?}, flags={flags})"),
            move || async move {
                let response = self
                    .fs
                    .read()
                    .await
                    .open(req.into(), path, parse_openflags(flags))
                    .await?;
                // TODO flags should be i32 and is in fuser, but fuse_mt accidentally converts it to u32. Undo that.
                let flags = convert_openflags(response.flags.into()) as u32;
                Ok((response.fh.0, flags))
            },
        )
    }

    fn read(
        &self,
        req: RequestInfo,
        path: &Path,
        fh: u64,
        offset: u64,
        size: u32,
        callback: impl FnOnce(ResultSlice<'_>) -> CallbackResult,
    ) -> CallbackResult {
        // TODO Is it ok to call block_on concurrently for multiple fs operations? Probably not.
        self.runtime.block_on(async move {
            let log_msg = format!("read({path:?}, fh={fh}, offset={offset}, size={size})");
            log::info!("{}...", log_msg);
            self.fs
                .read()
                .await
                .read(
                    req.into(),
                    path,
                    FileHandle::from(fh),
                    NumBytes::from(offset),
                    NumBytes::from(u64::from(size)),
                    move |slice| match slice {
                        Ok(slice) => {
                            let result = callback(Ok(slice));
                            log::info!("{}...done", log_msg);
                            result
                        }
                        Err(err) => {
                            let result = callback(Err(err.system_error_code()));
                            log::info!("{}...failed: {err:?}", log_msg);
                            result
                        }
                    },
                )
                .await
        })
    }

    fn write(
        &self,
        req: RequestInfo,
        path: &Path,
        fh: u64,
        offset: u64,
        data: Vec<u8>,
        flags: u32,
    ) -> ResultWrite {
        self.run_async(
            &format!(
                "write({path:?}, fh={fh}, offset={offset}, data=[{data_len} bytes], flags={flags})",
                data_len = data.len(),
            ),
            move || async move {
                let response = self
                    .fs
                    .read()
                    .await
                    .write(
                        req.into(),
                        path,
                        FileHandle::from(fh),
                        NumBytes::from(offset),
                        data,
                        flags,
                    )
                    .await?;
                // TODO No unwrap
                Ok(u32::try_from(u64::from(response)).unwrap())
            },
        )
    }

    fn flush(&self, req: RequestInfo, path: &Path, fh: u64, lock_owner: u64) -> ResultEmpty {
        self.run_async(&format!("flush({path:?}, fh={fh})"), || async move {
            self.fs
                .read()
                .await
                .flush(req.into(), path, FileHandle::from(fh), lock_owner)
                .await
        })
    }

    fn release(
        &self,
        req: RequestInfo,
        path: &Path,
        fh: u64,
        flags: u32,
        lock_owner: u64,
        flush: bool,
    ) -> ResultEmpty {
        // TODO flags should be i32 and is in fuser, but fuse_mt accidentally converts it to u32. Undo that.
        let flags = flags as i32;
        self.run_async(
            &format!(
                "release({path:?}, fh={fh}, flags={flags}, lock_owner={lock_owner}, flush={flush})"
            ),
            || async move {
                self.fs
                    .read()
                    .await
                    .release(
                        req.into(),
                        path,
                        FileHandle::from(fh),
                        parse_openflags(flags),
                        lock_owner,
                        flush,
                    )
                    .await
            },
        )
    }

    fn fsync(&self, req: RequestInfo, path: &Path, fh: u64, datasync: bool) -> ResultEmpty {
        self.run_async(
            &format!("fsync({path:?}, fh={fh}, datasync={datasync})"),
            || async move {
                self.fs
                    .read()
                    .await
                    .fsync(req.into(), path, FileHandle::from(fh), datasync)
                    .await
            },
        )
    }

    fn opendir(&self, req: RequestInfo, path: &Path, flags: u32) -> ResultOpen {
        self.run_async(
            &format!("opendir({path:?}, flags={flags})"),
            move || async move {
                let response = self
                    .fs
                    .read()
                    .await
                    .opendir(req.into(), path, flags)
                    .await?;
                Ok((response.fh.0, response.flags))
            },
        )
    }

    fn readdir(&self, req: RequestInfo, path: &Path, fh: u64) -> ResultReaddir {
        self.run_async(&format!("readdir({path:?}, fh={fh})"), move || async move {
            let entries = self
                .fs
                .read()
                .await
                .readdir(req.into(), path, FileHandle::from(fh))
                .await?;
            Ok(convert_dir_entries(entries))
        })
    }

    fn releasedir(&self, req: RequestInfo, path: &Path, fh: u64, flags: u32) -> ResultEmpty {
        self.run_async(
            &format!("releasedir({path:?}, fh={fh}, flags={flags})"),
            || async move {
                self.fs
                    .read()
                    .await
                    .releasedir(req.into(), path, FileHandle::from(fh), flags)
                    .await
            },
        )
    }

    fn fsyncdir(&self, req: RequestInfo, path: &Path, fh: u64, datasync: bool) -> ResultEmpty {
        self.run_async(
            &format!("fsyncdir({path:?}, fh={fh}, datasync={datasync})"),
            || async move {
                self.fs
                    .read()
                    .await
                    .fsyncdir(req.into(), path, FileHandle::from(fh), datasync)
                    .await
            },
        )
    }

    fn statfs(&self, req: RequestInfo, path: &Path) -> ResultStatfs {
        self.run_async(&format!("statfs({path:?})"), move || async move {
            let response = self.fs.read().await.statfs(req.into(), path).await?;
            Ok(convert_statfs(response))
        })
    }

    fn setxattr(
        &self,
        req: RequestInfo,
        path: &Path,
        name: &OsStr,
        value: &[u8],
        flags: u32,
        position: u32,
    ) -> ResultEmpty {
        let name = &parse_node_name(name);
        self.run_async(
            &format!(
                "setxattr({path:?}, name={name:?}, value=[{value_len} bytes], flags={flags}, position={position})",
                value_len = value.len(),
            ),
            || async move {
                self.fs.read().await.setxattr(
                    req.into(),
                    path,
                    name,
                    value,
                    flags,
                    position,
                ).await
            },
        )
    }

    fn getxattr(&self, req: RequestInfo, path: &Path, name: &OsStr, size: u32) -> ResultXattr {
        self.run_async(
            &format!("getxattr({path:?}, name={name:?}, size={size})"),
            move || async move {
                let req = req.into();
                let name = parse_node_name(name);
                // fuse_mt wants us to return Xattr::Size if the `size` parameter is zero, and the data otherwise.
                if 0 == size {
                    let response = self
                        .fs
                        .read()
                        .await
                        .getxattr_numbytes(req, path, &name)
                        .await?;
                    // TODO No unwrap
                    Ok(Xattr::Size(u32::try_from(u64::from(response)).unwrap()))
                } else {
                    let response = self
                        .fs
                        .read()
                        .await
                        .getxattr_data(req, path, &name, NumBytes::from(u64::from(size)))
                        .await?;
                    Ok(Xattr::Data(response))
                }
            },
        )
    }

    fn listxattr(&self, req: RequestInfo, path: &Path, size: u32) -> ResultXattr {
        self.run_async(
            &format!("getxattr({path:?}, size={size})"),
            move || async move {
                let req = req.into();
                // fuse_mt wants us to return Xattr::Size if the `size` parameter is zero, and the data otherwise.
                if 0 == size {
                    let response = self.fs.read().await.listxattr_numbytes(req, path).await?;
                    // TODO No unwrap
                    Ok(Xattr::Size(u32::try_from(u64::from(response)).unwrap()))
                } else {
                    let response = self
                        .fs
                        .read()
                        .await
                        .listxattr_data(req, path, NumBytes::from(u64::from(size)))
                        .await?;
                    Ok(Xattr::Data(response))
                }
            },
        )
    }

    fn removexattr(&self, req: RequestInfo, path: &Path, name: &OsStr) -> ResultEmpty {
        let name = &parse_node_name(name);
        self.run_async(
            &format!("removexattr({path:?}, name={name:?})"),
            || async move {
                self.fs
                    .read()
                    .await
                    .removexattr(req.into(), path, name)
                    .await
            },
        )
    }

    fn access(&self, req: RequestInfo, path: &Path, mask: u32) -> ResultEmpty {
        self.run_async(&format!("access({path:?}, mask={mask})"), || async move {
            self.fs.read().await.access(req.into(), path, mask).await
        })
    }

    fn create(
        &self,
        req: RequestInfo,
        parent: &Path,
        name: &OsStr,
        mode: u32,
        flags: u32,
    ) -> ResultCreate {
        let flags = flags as i32;
        self.run_async(
            &format!("create({parent:?}, name={name:?}, mode={mode}, flags={flags})"),
            move || async move {
                let response = self
                    .fs
                    .read()
                    .await
                    .create(
                        req.into(),
                        parent,
                        &parse_node_name(name),
                        Mode::from(mode),
                        flags,
                    )
                    .await?;
                // TODO flags should be i32 and is in fuser, but fuse_mt accidentally converts it to u32. Undo that.
                let flags = response.flags as u32;
                Ok(CreatedEntry {
                    ttl: response.ttl,
                    attr: convert_node_attrs(response.attrs),
                    fh: response.fh.0,
                    flags,
                })
            },
        )
    }
}

fn convert_node_attrs(attrs: NodeAttrs) -> FileAttr {
    let size: u64 = attrs.num_bytes.into();
    FileAttr {
        size,
        blocks: attrs.num_blocks.unwrap_or(size / 512),
        atime: attrs.atime,
        mtime: attrs.mtime,
        ctime: attrs.ctime,
        crtime: attrs.ctime, // TODO actually store and compute crtime
        kind: convert_node_kind(attrs.mode.node_kind()),
        perm: convert_permission_bits(attrs.mode),
        nlink: attrs.nlink,
        uid: attrs.uid.into(),
        gid: attrs.gid.into(),
        /// Device ID (if special file)
        rdev: 0, // TODO What to do about this?
        /// Flags (macOS only; see chflags(2))
        flags: 0, // TODO What to do about this?
    }
}

impl<Fs> Drop for BackendAdapter<Fs>
where
    Fs: AsyncFilesystem + AsyncDrop<Error = FsError> + Debug + Send + Sync + 'static,
{
    fn drop(&mut self) {
        // TODO
        // if !self.fs.read().await.is_dropped() {
        //     safe_panic!("BackendAdapter dropped without calling destroy() first");
        // }
    }
}

fn convert_node_kind(kind: NodeKind) -> fuse_mt::FileType {
    match kind {
        NodeKind::File => fuse_mt::FileType::RegularFile,
        NodeKind::Dir => fuse_mt::FileType::Directory,
        NodeKind::Symlink => fuse_mt::FileType::Symlink,
    }
}

fn convert_permission_bits(mode: Mode) -> u16 {
    let mode_bits: u32 = mode.into();
    // TODO Is 0o777 the right mask or do we need 0o7777?
    let perm_bits = mode_bits & 0o777;
    perm_bits as u16
}

fn convert_dir_entries(entries: Vec<DirEntry>) -> Vec<fuse_mt::DirectoryEntry> {
    entries
        .into_iter()
        .map(|entry| fuse_mt::DirectoryEntry {
            name: entry.name.into(), // TODO Is into() the best way to convert from String to OsString?
            kind: convert_node_kind(entry.kind),
        })
        .collect()
}

fn parse_node_name(name: &OsStr) -> Cow<'_, str> {
    let name = name.to_string_lossy(); // TODO Is to_string_lossy the best way to convert from OsString to String?
    assert!(!name.contains('/'), "name must not contain '/': {name:?}");
    assert!(
        !name.contains('\0'),
        "name must not contain the null byte: {name:?}"
    );
    assert!(name != ".", "name cannot be '.'");
    assert!(name != "..", "name cannot be '..'");
    name
}

fn parse_openflags(flags: i32) -> OpenFlags {
    // TODO Is this the right way to parse openflags? Are there other flags than just Read+Write?
    //      https://docs.rs/fuser/latest/fuser/trait.Filesystem.html#method.open seems to suggest so.
    match flags & libc::O_ACCMODE {
        libc::O_RDONLY => OpenFlags::Read,
        libc::O_WRONLY => OpenFlags::Write,
        libc::O_RDWR => OpenFlags::ReadWrite,
        _ => panic!("invalid flags: {flags}"),
    }
}

fn convert_openflags(flags: OpenFlags) -> i32 {
    // TODO Is this the right way to convert openflags? Are there other flags than just Read+Write?
    //      https://docs.rs/fuser/latest/fuser/trait.Filesystem.html#method.open seems to suggest so.
    match flags {
        OpenFlags::Read => libc::O_RDONLY,
        OpenFlags::Write => libc::O_WRONLY,
        OpenFlags::ReadWrite => libc::O_RDWR,
    }
}

fn convert_statfs(statfs: Statfs) -> fuse_mt::Statfs {
    fuse_mt::Statfs {
        blocks: statfs.num_total_blocks,
        bfree: statfs.num_free_blocks,
        bavail: statfs.num_available_blocks,
        files: statfs.num_total_inodes,
        ffree: statfs.num_free_inodes,
        bsize: statfs.blocksize,
        namelen: statfs.max_filename_length,
        // TODO What is fragment size? Should it be different to blocksize?
        frsize: statfs.blocksize,
    }
}

impl From<fuse_mt::RequestInfo> for crate::low_level_api::RequestInfo {
    fn from(value: fuse_mt::RequestInfo) -> Self {
        Self {
            unique: value.unique,
            uid: Uid::from(value.uid),
            gid: Gid::from(value.gid),
            pid: value.pid,
        }
    }
}

trait IntoOptionFileHandle {
    fn into_fh(self) -> Option<FileHandle>;
}
impl IntoOptionFileHandle for Option<u64> {
    fn into_fh(self) -> Option<FileHandle> {
        self.map(FileHandle::from)
    }
}

trait IntoOptionUid {
    fn into_uid(self) -> Option<Uid>;
}
impl IntoOptionUid for Option<u32> {
    fn into_uid(self) -> Option<Uid> {
        self.map(Uid::from)
    }
}

trait IntoOptionGid {
    fn into_gid(self) -> Option<Gid>;
}
impl IntoOptionGid for Option<u32> {
    fn into_gid(self) -> Option<Gid> {
        self.map(Gid::from)
    }
}
