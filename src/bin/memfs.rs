use fuse_ll::fuse::{
    self, FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use libc::{EEXIST, EINVAL, EIO, EISDIR, ENODATA, ENOENT, ENOTDIR, ENOTEMPTY};
use log::{debug, error}; // info, warn
use nix::dir::{Dir, Type};
use nix::fcntl::{self, FcntlArg, OFlag};
use nix::sys::stat::{self, FileStat, Mode, SFlag};
use nix::sys::uio;
use nix::unistd::{self, Gid, Uid, UnlinkatFlags};
use std::cell::{Cell, RefCell};
use std::cmp;
use std::collections::{btree_map::Entry, BTreeMap, BTreeSet};
use std::convert::AsRef;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::ops::Drop;
use std::os::raw::c_int;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{self, AtomicI64};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MY_TTL_SEC: u64 = 1; // TODO: should be a long value, say 1 hour
const MY_GENERATION: u64 = 1;
// const MY_DIR_MODE: u16 = 0o755;
// const MY_FILE_MODE: u16 = 0o644;
const FUSE_ROOT_ID: u64 = 1; // defined in include/fuse_kernel.h

mod util {
    use super::*;

    pub fn parse_oflag(flags: u32) -> OFlag {
        debug_assert!(
            flags < std::i32::MAX as u32,
            format!(
                "helper_parse_oflag() found flags={} overflow, larger than u16::MAX",
                flags,
            ),
        );
        let oflags = OFlag::from_bits_truncate(flags as i32);
        debug!("helper_parse_oflag() read file flags: {:?}", oflags);
        oflags
    }

    pub fn parse_mode(mode: u32) -> Mode {
        debug_assert!(
            mode < std::u16::MAX as u32,
            format!(
                "helper_parse_mode() found mode={} overflow, larger than u16::MAX",
                mode,
            ),
        );
        let fmode = Mode::from_bits_truncate(mode as u16);
        debug!("helper_parse_mode() read file mode: {:?}", fmode);
        fmode
    }
    pub fn parse_sflag(flags: u32) -> SFlag {
        debug_assert!(
            flags < std::u16::MAX as u32,
            format!(
                "parse_sflag() found flags={} overflow, larger than u16::MAX",
                flags,
            ),
        );
        let sflag = SFlag::from_bits_truncate(flags as u16);
        debug!("convert_sflag() read file type as: {:?}", sflag);
        sflag
    }

    pub fn convert_sflag(sflag: SFlag) -> FileType {
        match sflag {
            SFlag::S_IFDIR => FileType::Directory,
            SFlag::S_IFREG => FileType::RegularFile,
            _ => panic!("convert_sflag() found unsupported file type: {:?}", sflag),
        }
    }

    pub fn convert_node_type(file_type: &Type) -> FileType {
        match file_type {
            Type::Directory => FileType::Directory,
            Type::File => FileType::RegularFile,
            _ => panic!(
                "helper_convert_node_type() found unsupported file type: {:?}",
                file_type,
            ),
        }
    }

    pub fn open_dir(path: &Path) -> Result<Dir, nix::Error> {
        let oflags = OFlag::O_RDONLY | OFlag::O_DIRECTORY;
        // let dfd = fcntl::open(path, oflags, Mode::empty())?;
        let dfd = Dir::open(path, oflags, Mode::empty())?;
        Ok(dfd)
    }

    pub fn open_dir_at(dir: &Dir, child_name: &OsStr) -> Result<Dir, nix::Error> {
        let oflags = OFlag::O_RDONLY | OFlag::O_DIRECTORY;
        let dir = Dir::openat(dir.as_raw_fd(), child_name, oflags, Mode::empty())?;
        Ok(dir)
    }

    pub fn read_attr(fd: RawFd) -> Result<FileAttr, nix::Error> {
        let st = stat::fstat(fd.clone())?;

        #[cfg(target_os = "macos")]
        fn build_crtime(st: &FileStat) -> Option<SystemTime> {
            UNIX_EPOCH.checked_add(Duration::new(
                st.st_birthtime as u64,
                st.st_birthtime_nsec as u32,
            ))
        }
        #[cfg(target_os = "linux")]
        fn build_crtime(st: &FileStat) -> Option<SystemTime> {
            None
        }

        let atime =
            UNIX_EPOCH.checked_add(Duration::new(st.st_atime as u64, st.st_atime_nsec as u32));
        let mtime =
            UNIX_EPOCH.checked_add(Duration::new(st.st_mtime as u64, st.st_mtime_nsec as u32));
        let ctime =
            UNIX_EPOCH.checked_add(Duration::new(st.st_ctime as u64, st.st_ctime_nsec as u32));
        let crtime = build_crtime(&st);

        let perm = util::parse_mode(st.st_mode as u32).bits();
        debug!("helper_read_attr() got file permission as: {}", perm);
        let sflag = util::parse_sflag(st.st_mode as u32);
        let kind = util::convert_sflag(sflag);

        let nt = SystemTime::now();
        let attr = FileAttr {
            ino: st.st_ino,
            size: st.st_size as u64,
            blocks: st.st_blocks as u64,
            atime: atime.unwrap_or(nt),
            mtime: mtime.unwrap_or(nt),
            ctime: ctime.unwrap_or(nt),
            crtime: crtime.unwrap_or(nt),
            kind: kind,
            perm: perm,
            nlink: st.st_nlink as u32,
            uid: st.st_uid,
            gid: st.st_gid,
            rdev: st.st_rdev as u32,
            flags: st.st_flags,
        };
        Ok(attr)
    }
}

#[derive(Debug)]
struct DirEntry {
    ino: u64,
    name: OsString,
    entry_type: Type,
}

#[derive(Debug)]
struct DirNode {
    parent: u64,
    name: OsString,
    path: PathBuf,
    attr: Cell<FileAttr>,
    data: RefCell<BTreeMap<OsString, DirEntry>>,
    dir_fd: RefCell<Dir>,
    open_count: AtomicI64,
    lookup_count: AtomicI64,
}

#[derive(Debug)]
struct FileNode {
    parent: u64,
    name: OsString,
    path: PathBuf,
    attr: Cell<FileAttr>,
    data: RefCell<Vec<u8>>,
    fd: RawFd,
    open_count: AtomicI64,
    lookup_count: AtomicI64,
}

impl Drop for FileNode {
    fn drop(&mut self) {
        unistd::close(self.fd).expect(&format!(
            "FileNode::drop() failed to clode the file handler of
                file name {:?} ino={}",
            self.name,
            self.attr.get_mut().ino,
        ));
    }
}

#[derive(Debug)]
enum INode {
    DIR(DirNode),
    FILE(FileNode),
}

impl INode {
    fn helper_get_dir_node(&self) -> &DirNode {
        match self {
            INode::DIR(dir_node) => dir_node,
            INode::FILE(_) => panic!("helper_get_dir_node() cannot read FileNode"),
        }
    }

    fn helper_get_file_node(&self) -> &FileNode {
        match self {
            INode::DIR(_) => panic!("helper_get_file_node() cannot read DirNode"),
            INode::FILE(file_node) => file_node,
        }
    }

    fn get_ino(&self) -> u64 {
        self.get_attr().ino
    }

    fn get_parent_ino(&self) -> u64 {
        match self {
            INode::DIR(dir_node) => dir_node.parent,
            INode::FILE(file_node) => file_node.parent,
        }
    }

    fn get_name(&self) -> &OsString {
        match self {
            INode::DIR(dir_node) => &dir_node.name,
            INode::FILE(file_node) => &file_node.name,
        }
    }

    fn get_type(&self) -> Type {
        match self {
            INode::DIR(_) => Type::Directory,
            INode::FILE(_) => Type::File,
        }
    }

    fn get_attr(&self) -> FileAttr {
        match self {
            INode::DIR(dir_node) => dir_node.attr.get(),
            INode::FILE(file_node) => file_node.attr.get(),
        }
    }

    fn lookup_attr(&self, func: impl FnOnce(&FileAttr)) {
        let attr = match self {
            INode::DIR(dir_node) => {
                let attr = dir_node.attr.get();
                debug_assert_eq!(attr.kind, FileType::Directory);
                attr
            }
            INode::FILE(file_node) => {
                let attr = file_node.attr.get();
                debug_assert_eq!(attr.kind, FileType::RegularFile);
                attr
            }
        };
        func(&attr);
        self.inc_lookup_count();
    }

    fn set_attr(&mut self, func: impl FnOnce(&mut FileAttr)) {
        match self {
            INode::DIR(dir_node) => {
                let attr = dir_node.attr.get_mut();
                debug_assert_eq!(attr.kind, FileType::Directory);
                func(attr);
            }
            INode::FILE(file_node) => {
                let attr = file_node.attr.get_mut();
                debug_assert_eq!(attr.kind, FileType::RegularFile);
                func(attr);
            }
        }
    }

    fn inc_open_count(&self) -> i64 {
        match self {
            INode::DIR(dir_node) => dir_node.open_count.fetch_add(1, atomic::Ordering::SeqCst),
            INode::FILE(file_node) => file_node.open_count.fetch_add(1, atomic::Ordering::SeqCst),
        }
    }

    fn dec_open_count(&self) -> i64 {
        match self {
            INode::DIR(dir_node) => dir_node.open_count.fetch_sub(1, atomic::Ordering::SeqCst),
            INode::FILE(file_node) => file_node.open_count.fetch_sub(1, atomic::Ordering::SeqCst),
        }
    }

    fn get_open_count(&self) -> i64 {
        match self {
            INode::DIR(dir_node) => dir_node.open_count.load(atomic::Ordering::SeqCst),
            INode::FILE(file_node) => file_node.open_count.load(atomic::Ordering::SeqCst),
        }
    }

    fn inc_lookup_count(&self) -> i64 {
        match self {
            INode::DIR(dir_node) => dir_node.lookup_count.fetch_add(1, atomic::Ordering::SeqCst),
            INode::FILE(file_node) => file_node
                .lookup_count
                .fetch_add(1, atomic::Ordering::SeqCst),
        }
    }

    fn dec_lookup_count_by(&self, nlookup: u64) -> i64 {
        debug_assert!(nlookup < std::i64::MAX as u64);
        match self {
            INode::DIR(dir_node) => dir_node
                .lookup_count
                .fetch_sub(nlookup as i64, atomic::Ordering::SeqCst),
            INode::FILE(file_node) => file_node
                .lookup_count
                .fetch_sub(nlookup as i64, atomic::Ordering::SeqCst),
        }
    }

    fn get_lookup_count(&self) -> i64 {
        match self {
            INode::DIR(dir_node) => dir_node.lookup_count.load(atomic::Ordering::SeqCst),
            INode::FILE(file_node) => file_node.lookup_count.load(atomic::Ordering::SeqCst),
        }
    }

    fn get_entry(&self, name: &OsString) -> Option<DirEntry> {
        let parent_node = self.helper_get_dir_node();
        match parent_node.data.borrow().get(name) {
            // TODO: how to return value within RefCell without copy explicitly
            Some(dir_entry) => Some(DirEntry {
                ino: dir_entry.ino,
                name: dir_entry.name.clone(),
                entry_type: dir_entry.entry_type,
            }),
            None => None,
        }
    }

    fn open_root_inode(root_ino: u64, name: OsString, path: PathBuf) -> INode {
        let dir_fd = util::open_dir(&path).expect(&format!(
            "new_dir_inode() failed to open directory {:?}",
            path,
        ));
        let mut attr = util::read_attr(dir_fd.as_raw_fd()).expect(&format!(
            "new_dir_inode() failed to read directory attribute {:?}",
            path,
        ));
        attr.ino = root_ino; // replace root ino with 1

        // lookup count and open count are increased to 1 by creation
        let root_inode = INode::DIR(DirNode {
            parent: root_ino,
            name,
            path,
            attr: Cell::new(attr),
            data: RefCell::new(BTreeMap::new()),
            dir_fd: RefCell::new(dir_fd),
            open_count: AtomicI64::new(1),
            lookup_count: AtomicI64::new(1),
        });

        if root_inode.need_load_data() {
            root_inode.helper_load_dir_data();
        }

        root_inode
    }

    fn helper_open_child_dir(
        &self,
        child_dir_name: &OsString,
        mode: Mode,
        create_dir: bool,
    ) -> INode {
        let parent_node = self.helper_get_dir_node();
        let parent = self.get_ino();

        if create_dir {
            stat::mkdirat(
                parent_node.dir_fd.borrow().as_raw_fd(),
                &PathBuf::from(child_dir_name),
                mode,
            )
            .expect(&format!(
                "helper_open_child_dir() failed to create directory name={:?} under parent ino={}",
                child_dir_name, parent,
            ));
        }

        let child_dir_fd =
            util::open_dir_at(&parent_node.dir_fd.borrow(), child_dir_name).expect(&format!(
                "helper_open_child_dir() failed to open the new directory name={:?}
                    under parent ino={}",
                child_dir_name, parent,
            ));
        let child_raw_fd = child_dir_fd.as_raw_fd();

        // get new directory attribute
        let child_attr = util::read_attr(child_raw_fd).expect(&format!(
            "helper_open_child_dir() failed to get the attribute of the new child directory"
        ));
        debug_assert_eq!(FileType::Directory, child_attr.kind);

        if create_dir {
            // insert new entry to parent directory
            // TODO: support thread-safe
            let parent_data = &mut *parent_node.data.borrow_mut();
            let previous_value = parent_data.insert(
                child_dir_name.clone(),
                DirEntry {
                    ino: child_attr.ino,
                    name: child_dir_name.clone(),
                    entry_type: Type::Directory,
                },
            );
            debug_assert!(previous_value.is_none());
        }

        // lookup count and open count are increased to 1 by creation
        let child_inode = INode::DIR(DirNode {
            parent,
            name: child_dir_name.clone(),
            path: parent_node.path.join(&Path::new(child_dir_name)),
            attr: Cell::new(child_attr),
            data: RefCell::new(BTreeMap::new()),
            dir_fd: RefCell::new(child_dir_fd),
            open_count: AtomicI64::new(1),
            lookup_count: AtomicI64::new(1),
        });

        if child_inode.need_load_data() {
            child_inode.helper_load_dir_data();
        }

        child_inode
    }

    fn open_child_dir(&self, child_dir_name: &OsString) -> INode {
        self.helper_open_child_dir(child_dir_name, Mode::empty(), false)
    }

    fn create_child_dir(&self, child_dir_name: &OsString, mode: Mode) -> INode {
        self.helper_open_child_dir(child_dir_name, mode, true)
    }

    fn helper_load_dir_data(&self) {
        let dir_node = self.helper_get_dir_node();
        let entry_count = dir_node
            .dir_fd
            .borrow_mut()
            .iter()
            .map(|e| e.expect(&format!("helper_load_dir_data() failed to load entry")))
            .filter(|e| {
                let bytes = e.file_name().to_bytes();
                !bytes.starts_with(&[b'.']) // skip hidden entries, '.' and '..'
            })
            .filter(|e| match e.file_type() {
                Some(t) => match t {
                    Type::Fifo => false,
                    Type::CharacterDevice => false,
                    Type::Directory => true,
                    Type::BlockDevice => false,
                    Type::File => true,
                    Type::Symlink => false,
                    Type::Socket => false,
                },
                None => false,
            })
            .map(|e| {
                let name = OsString::from(OsStr::from_bytes(e.file_name().to_bytes()));
                dir_node.data.borrow_mut().insert(
                    name.clone(),
                    DirEntry {
                        ino: e.ino(),
                        name,
                        entry_type: e.file_type().unwrap(), // safe to use unwrap() here
                    },
                )
            })
            .count();
        debug!(
            "helper_load_dir_data() successfully load {} directory entries",
            entry_count,
        );
    }

    fn helper_load_file_data(&self) {
        let file_node = self.helper_get_file_node();
        let ino = self.get_ino();
        let fd = file_node.fd;
        let file_size = file_node.attr.get().size;
        let file_data: &mut Vec<u8> = &mut file_node.data.borrow_mut();
        file_data.reserve(file_size as usize);
        unsafe {
            file_data.set_len(file_data.capacity());
        }
        let res = unistd::read(fd.clone(), &mut *file_data);
        match res {
            Ok(s) => unsafe {
                file_data.set_len(s as usize);
            },
            Err(e) => {
                panic!(
                    "helper_load_file_data() failed to
                        read the file of ino={} from disk, the error is: {:?}",
                    ino, e,
                );
            }
        }
        debug_assert_eq!(file_data.len(), file_size as usize);
        debug!(
            "helper_load_file_data() successfully load {} byte data",
            file_size,
        );
    }

    // to open child, parent dir must have been opened
    fn helper_open_child_file(
        &self,
        child_file_name: &OsString,
        oflags: OFlag,
        mode: Mode,
        create_file: bool,
    ) -> INode {
        let parent_node = self.helper_get_dir_node();
        let parent = self.get_ino();

        if create_file {
            debug_assert!(oflags.contains(OFlag::O_CREAT));
        }
        let child_fd = fcntl::openat(
            parent_node.dir_fd.borrow().as_raw_fd(),
            &PathBuf::from(child_file_name),
            oflags,
            mode,
        )
        .expect(&format!(
            "helper_open_child_file() failed to open a file name={:?}
                under parent ino={} with oflags: {:?} and mode: {:?}",
            child_file_name, parent, oflags, mode,
        ));

        // get new file attribute
        let child_attr = util::read_attr(child_fd).expect(&format!(
            "helper_open_child_file() failed to get the attribute of the new child"
        ));
        debug_assert_eq!(FileType::RegularFile, child_attr.kind);

        if create_file {
            // insert new entry to parent directory
            // TODO: support thread-safe
            let parent_data = &mut *parent_node.data.borrow_mut();
            let previous_value = parent_data.insert(
                child_file_name.clone(),
                DirEntry {
                    ino: child_attr.ino,
                    name: child_file_name.clone(),
                    entry_type: Type::File,
                },
            );
            debug_assert!(previous_value.is_none());
        }

        // lookup count and open count are increased to 1 by creation
        INode::FILE(FileNode {
            parent,
            name: child_file_name.clone(),
            path: parent_node.path.join(&Path::new(child_file_name)),
            attr: Cell::new(child_attr),
            data: RefCell::new(Vec::new()),
            fd: child_fd,
            open_count: AtomicI64::new(1),
            lookup_count: AtomicI64::new(1),
        })
    }

    fn open_child_file(&self, child_file_name: &OsString, oflags: OFlag) -> INode {
        self.helper_open_child_file(child_file_name, oflags, Mode::empty(), false)
    }

    fn create_child_file(&self, child_file_name: &OsString, oflags: OFlag, mode: Mode) -> INode {
        self.helper_open_child_file(child_file_name, oflags, Mode::empty(), true)
    }

    fn dup_fd(&self, oflags: OFlag) -> RawFd {
        let raw_fd: RawFd;
        match self {
            INode::DIR(dir_node) => {
                raw_fd = dir_node.dir_fd.borrow().as_raw_fd();
            }
            INode::FILE(file_node) => {
                raw_fd = file_node.fd;
            }
        }
        let ino = self.get_ino();
        let new_fd = unistd::dup(raw_fd).expect(&format!(
            "dup_fd() failed to duplicate the handler ino={} raw fd={:?}",
            ino, raw_fd,
        ));
        // let fcntl_oflags = FcntlArg::F_SETFL(oflags);
        // fcntl::fcntl(new_fd, fcntl_oflags).expect(&format!(
        //     "dup_fd() failed to set the flags {:?} of duplicated handler of ino={}",
        //     oflags, ino,
        // ));
        unistd::dup3(raw_fd, new_fd, oflags).expect(&format!(
            "dup_fd() failed to set the flags {:?} of duplicated handler of ino={}",
            oflags, ino,
        ));
        self.inc_open_count();
        new_fd
    }

    fn unlink_entry(&self, child_name: &OsString) -> Option<DirEntry> {
        let parent_node = self.helper_get_dir_node();
        let parent_data: &BTreeMap<OsString, DirEntry> = &parent_node.data.borrow();
        let child_entry = parent_data.get(child_name).expect(&format!(
            "unlink_entry() failed to find entry name: {:?}",
            child_name,
        ));
        // delete from disk and close the handler
        match child_entry.entry_type {
            Type::Directory => {
                unistd::unlinkat(
                    Some(parent_node.dir_fd.borrow().as_raw_fd()),
                    &PathBuf::from(child_name),
                    UnlinkatFlags::RemoveDir,
                )
                .expect(&format!(
                    "unlink_entry() failed to delete the file name {:?} from disk",
                    child_name,
                ));
            }
            Type::File => {
                unistd::unlinkat(
                    Some(parent_node.dir_fd.borrow().as_raw_fd()),
                    &PathBuf::from(child_name),
                    UnlinkatFlags::NoRemoveDir,
                )
                .expect(&format!(
                    "unlink_entry() failed to delete the file name {:?} from disk",
                    child_name,
                ));
            }
            _ => panic!(
                "unlink_entry() found unsupported entry type: {:?}",
                child_entry.entry_type
            ),
        }
        parent_node.data.borrow_mut().remove(child_name)
    }

    fn is_empty(&self) -> bool {
        match self {
            INode::DIR(dir_node) => dir_node.data.borrow().is_empty(),
            INode::FILE(file_node) => file_node.data.borrow().is_empty(),
        }
    }

    fn need_load_data(&self) -> bool {
        if !self.is_empty() {
            debug!(
                "need_load_data() found node data of name: {:?} and ino={} is in cache, no need to load",
                self.get_name(),
                self.get_ino(),
            );
            false
        } else if self.get_attr().size > 0 {
            debug!(
                "need_load_data() found node size of name: {:?} and ino={} is non-zero, need to load",
                self.get_name(),
                self.get_ino(),
            );
            true
        } else {
            debug!(
                "need_load_data() found node size of name: {:?} and ino={} is zero, no need to load",
                self.get_name(),
                self.get_ino(),
            );
            false
        }
    }

    fn read_dir(&self, func: impl FnOnce(&BTreeMap<OsString, DirEntry>)) {
        let dir_node = self.helper_get_dir_node();
        if self.need_load_data() {
            self.helper_load_dir_data();
        }
        func(&dir_node.data.borrow());
    }

    fn read_file(&self, func: impl FnOnce(&Vec<u8>)) {
        let file_node = self.helper_get_file_node();
        if self.need_load_data() {
            self.helper_load_file_data();
        }
        func(&file_node.data.borrow());
    }

    fn write_file(&mut self, fh: u64, offset: i64, data: &[u8], oflags: OFlag) -> usize {
        let file_node = match self {
            INode::DIR(_) => panic!("write_file() cannot write DirNode"),
            INode::FILE(file_node) => file_node,
        };
        let attr = file_node.attr.get_mut();
        let ino = attr.ino;
        let file_data = file_node.data.get_mut();

        let size_after_write = offset as usize + data.len();
        if file_data.capacity() < size_after_write {
            let before_cap = file_data.capacity();
            let extra_space_size = size_after_write - file_data.capacity();
            file_data.reserve(extra_space_size);
            // TODO: handle OOM when reserving
            // let result = file_data.try_reserve(extra_space_size);
            // if result.is_err() {
            //     warn!(
            //         "write cannot reserve enough space, the space size needed is {} byte",
            //         extra_space_size);
            //     reply.error(ENOMEM);
            //     return;
            // }
            debug!(
                "write_file() enlarged the file data vector capacity from {} to {}",
                before_cap,
                file_data.capacity(),
            );
        }
        match file_data.len().cmp(&(offset as usize)) {
            cmp::Ordering::Greater => {
                file_data.truncate(offset as usize);
                debug!(
                    "write() truncated the file of ino={} to size={}",
                    ino, offset
                );
            }
            cmp::Ordering::Less => {
                let zero_padding_size = (offset as usize) - file_data.len();
                let mut zero_padding_vec = vec![0u8; zero_padding_size];
                file_data.append(&mut zero_padding_vec);
            }
            cmp::Ordering::Equal => (),
        }
        file_data.extend_from_slice(data);

        let fcntl_oflags = FcntlArg::F_SETFL(oflags);
        let fd = fh as RawFd;
        fcntl::fcntl(fd, fcntl_oflags).expect(&format!(
            "write_file() failed to set the flags {:?} to file handler {} of ino={}",
            oflags, fd, ino,
        ));
        // TODO: async write to disk
        let written_size = uio::pwrite(fd, data, offset).expect("write() failed to write to disk");
        debug_assert_eq!(data.len(), written_size);

        // update the attribute of the written file
        attr.size = file_data.len() as u64;
        let ts = SystemTime::now();
        attr.mtime = ts;

        written_size
    }
}

struct MemoryFilesystem {
    // max_ino: AtomicU64,
    uid: Uid,
    gid: Gid,
    cache: BTreeMap<u64, INode>,
    trash: BTreeSet<u64>,
}

impl MemoryFilesystem {
    fn helper_create_node(
        &mut self,
        parent: u64,
        node_name: &OsString,
        mode: u32,
        node_type: Type,
        reply: ReplyEntry,
    ) {
        let node_kind = util::convert_node_type(&node_type);
        // pre-check
        let parent_inode = self.cache.get(&parent).expect(&format!(
            "helper_create_node() found fs is inconsistent,
                parent of ino={} should be in cache before create it new child",
            parent,
        ));
        if let Some(occupied) = parent_inode.get_entry(node_name) {
            debug!(
                "helper_create_node() found the directory of ino={}
                    already exists a child with name {:?} and ino={}",
                parent, node_name, occupied.ino,
            );
            reply.error(EEXIST);
            return;
        }
        // all checks are passed, ready to create new node
        let mflags = util::parse_mode(mode);
        let new_ino: u64;
        let new_inode: INode;
        match node_kind {
            FileType::Directory => {
                debug!(
                    "helper_create_node() about to create a directory with name={:?}, mode={:?}",
                    node_name, mflags,
                );
                new_inode = parent_inode.create_child_dir(node_name, mflags);
            }
            FileType::RegularFile => {
                let oflags = OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_RDWR;
                debug!(
                    "helper_create_node() about to
                        create a file with name={:?}, oflags={:?}, mode={:?}",
                    node_name, oflags, mflags,
                );
                new_inode = parent_inode.create_child_file(node_name, oflags, mflags);
            }
            _ => panic!(
                "helper_create_node() found unsupported file type: {:?}",
                node_kind
            ),
        }
        new_ino = new_inode.get_ino();
        let new_attr = new_inode.get_attr();
        self.cache.insert(new_ino, new_inode);

        let ttl = Duration::new(MY_TTL_SEC, 0);
        reply.entry(&ttl, &new_attr, MY_GENERATION);
        debug!(
            "helper_create_node() successfully created the new child name={:?}
                of ino={} under parent ino={}",
            node_name, new_ino, parent,
        );
    }

    fn helper_get_parent_inode(&self, ino: u64) -> &INode {
        let inode = self.cache.get(&ino).expect(&format!(
            "helper_get_parent_inode() failed to find the i-node of ino={}",
            ino,
        ));
        let parent_ino = inode.get_parent_ino();
        self.cache.get(&parent_ino).expect(&format!(
            "helper_get_parent_inode() failed to find the parent of ino={} for i-node of ino={}",
            parent_ino, ino,
        ))
    }

    fn helper_unlink_node_by_ino(&mut self, ino: u64) -> INode {
        let inode = self.cache.get(&ino).expect(&format!(
            "helper_unlink_node_by_ino() failed to find the i-node of ino={}",
            ino,
        ));
        let node_name = inode.get_name();

        let parent_inode = self.helper_get_parent_inode(ino);
        parent_inode.unlink_entry(node_name);

        let inode = self.cache.remove(&ino).unwrap();
        inode
    }

    fn helper_remove_node(
        &mut self,
        parent: u64,
        node_name: &OsString,
        node_type: Type,
        reply: ReplyEmpty,
    ) {
        let node_kind = util::convert_node_type(&node_type);
        let node_ino: u64;
        {
            // pre-checks
            let parent_inode = self.cache.get(&parent).expect(&format!(
                "helper_remove_node() found fs is inconsistent,
                    parent of ino={} should be in cache before remove its child",
                parent,
            ));
            match parent_inode.get_entry(node_name) {
                None => {
                    debug!(
                        "helper_remove_node() failed to find node name={:?}
                            under parent of ino={}",
                        node_name, parent,
                    );
                    reply.error(ENOENT);
                    return;
                }
                Some(child_entry) => {
                    node_ino = child_entry.ino;
                    if let FileType::Directory = node_kind {
                        // check the directory to delete is empty
                        let dir_inode = self.cache.get(&node_ino).expect(&format!(
                            "helper_remove_node() found fs is inconsistent,
                                directory name={:?} of ino={} found under the parent of ino={},
                                but no i-node found for this directory",
                            node_name, node_ino, parent,
                        ));
                        if !dir_inode.is_empty() {
                            debug!(
                                "helper_remove_node() cannot remove
                                    the non-empty directory name={:?} of ino={}
                                    under the parent directory of ino={}",
                                node_name, node_ino, parent,
                            );
                            reply.error(ENOTEMPTY);
                            return;
                        }
                    }

                    let child_inode = self.cache.get(&node_ino).expect(&format!(
                        "helper_remove_node() found fs is inconsistent, node name={:?} of ino={}
                            found under the parent of ino={}, but no i-node found for this node",
                        node_name, node_ino, parent,
                    ));
                    debug_assert_eq!(node_ino, child_inode.get_ino());
                    debug_assert_eq!(node_name, child_inode.get_name());
                    debug_assert_eq!(parent, child_inode.get_parent_ino());
                    debug_assert_eq!(node_type, child_inode.get_type());
                    debug_assert_eq!(node_kind, child_inode.get_attr().kind);
                }
            }
        }
        {
            // all checks passed, ready to remove, safe to use unwrap() below,
            // except in multi-thread case
            // TODO: when deferred deletion, remove entry from directory first
            // let child_entry = parent_inode.unlink_entry(node_name).unwrap();

            let mut defered_deletion = false;
            {
                let inode = self.cache.get(&node_ino).expect(&format!(
                    "helper_remove_node() failed to find the i-node of ino={}",
                    node_ino,
                ));
                debug_assert!(inode.get_lookup_count() >= 0); // lookup count cannot be negative
                if inode.get_lookup_count() > 0 {
                    defered_deletion = true;
                }
            }
            if defered_deletion {
                let inode = self.cache.get(&node_ino).unwrap(); // TODO: support thread-safe
                let insert_result = self.trash.insert(node_ino);
                debug_assert!(insert_result); // check thread-safe in case of duplicated deferred deletion requests
                debug!(
                    "helper_remove_node() defered removed the node name={:?} of ino={}
                        under parent ino={}, its attr is: {:?}, open count is: {}, lookup count is : {}",
                    node_name,
                    node_ino,
                    parent,
                    INode::get_attr(inode),
                    INode::get_open_count(inode),
                    INode::get_lookup_count(inode),
                );
            } else {
                let inode = self.helper_unlink_node_by_ino(node_ino);
                debug!(
                    "helper_remove_node() successfully removed the node name={:?} of ino={}
                        under parent ino={}, its attr is: {:?}, open count is: {}, lookup count is : {}",
                    node_name,
                    node_ino,
                    parent,
                    INode::get_attr(&inode),
                    INode::get_open_count(&inode),
                    INode::get_lookup_count(&inode),
                );
            }
            reply.ok();
        }
    }

    fn new<P: AsRef<Path>>(mount_point: P) -> MemoryFilesystem {
        let uid = unistd::getuid();
        let gid = unistd::getgid();

        let mount_dir = PathBuf::from(mount_point.as_ref());
        if !mount_dir.is_dir() {
            panic!("the input mount path is not a directory");
        }
        let root_path = fs::canonicalize(&mount_dir).expect(&format!(
            "failed to convert the mount point {:?} to a full path",
            mount_dir,
        ));

        let root_inode = INode::open_root_inode(FUSE_ROOT_ID, OsString::from("/"), root_path);
        let mut cache = BTreeMap::new();
        cache.insert(FUSE_ROOT_ID, root_inode);
        let trash = BTreeSet::new(); // for deferred deletion

        MemoryFilesystem {
            uid,
            gid,
            cache,
            trash,
        }
    }
}

impl Filesystem for MemoryFilesystem {
    fn init(&mut self, _req: &Request<'_>) -> Result<(), c_int> {
        // TODO: test fd health without using unwrap()
        // let dir_inode = self.cache.get(&FUSE_ROOT_ID).unwrap();
        // let dir_fd = INode::get_dir_fd_mut(&dir_inode);
        // dir_fd.iter().map(|e| dbg!(e)).count();
        // let attr = util::read_attr(dir_fd.as_raw_fd()).unwrap();
        // dbg!(attr);
        // let sub_dir = PathBuf::from("文件夹1");
        // let sub_file = PathBuf::from("文件A.txt");
        // let dfd = util::open_dir_at(&dir_fd, sub_dir.as_os_str()).unwrap();
        // let ffd = util::open_file_at(
        //     &dir_fd,
        //     sub_file.as_os_str(),
        //     OFlag::O_RDWR.bits() as u32,
        // )
        // .unwrap();
        // let file_attr = util::read_attr(ffd).unwrap();
        // dbg!(file_attr);
        // let dir_attr = util::read_attr(dfd.as_raw_fd()).unwrap();
        // dbg!(dir_attr);
        // let dup_ffd = unistd::dup(ffd).unwrap();
        // let file_attr = util::read_attr(dup_ffd).unwrap();
        // dbg!(file_attr);
        // let dup_dfd = unistd::dup(dfd.as_raw_fd()).unwrap();
        // let mut dup_dir_fd = Dir::from_fd(dup_dfd).unwrap();
        // let dir_attr = util::read_attr(dup_dir_fd.as_raw_fd()).unwrap();
        // dbg!(dir_attr);
        // let dir_data = util::build_dir_data(&mut dup_dir_fd).unwrap();
        // dbg!(dir_data);

        Ok(())
    }

    fn getattr(&mut self, req: &Request, ino: u64, reply: ReplyAttr) {
        debug!("getattr(ino={}, req={:?})", ino, req.request);

        let inode = self.cache.get(&ino).expect(&format!(
            "getattr() found fs is inconsistent, the i-node of ino={} should be in cache",
            ino,
        ));
        let attr = inode.get_attr();
        debug!(
            "getattr() cache hit when searching the attribute of ino={}",
            ino,
        );
        let ttl = Duration::new(MY_TTL_SEC, 0);
        reply.attr(&ttl, &attr);
        debug!(
            "getattr() successfully got the attribute of ino={}, the attr is: {:?}",
            ino, &attr,
        );
    }

    // The order of calls is:
    //     init
    //     ...
    //     opendir
    //     readdir
    //     releasedir
    //     open
    //     read
    //     write
    //     ...
    //     flush
    //     release
    //     ...
    //     destroy
    fn open(&mut self, req: &Request<'_>, ino: u64, flags: u32, reply: ReplyOpen) {
        debug!("open(ino={}, flags={}, req={:?})", ino, flags, req.request,);
        let inode = self.cache.get(&ino).expect(&format!(
            "open() found fs is inconsistent, the i-node of ino={} should be in cache",
            ino,
        ));
        let oflags = util::parse_oflag(flags);
        let new_fd = inode.dup_fd(oflags);
        reply.opened(new_fd as u64, flags);
        debug!(
            "open() successfully duplicated the file handler of ino={}, fd={}, flags: {:?}",
            ino, new_fd, flags,
        );
    }

    fn release(
        &mut self,
        req: &Request<'_>,
        ino: u64,
        fh: u64,
        flags: u32,
        lock_owner: u64,
        flush: bool,
        reply: ReplyEmpty,
    ) {
        debug!(
            "release(ino={}, fh={}, flags={}, lock_owner={}, flush={}, req={:?})",
            ino, fh, flags, lock_owner, flush, req.request,
        );
        let inode = self.cache.get(&ino).expect(&format!(
            "release() found fs is inconsistent, the i-node of ino={} should be in cache",
            ino,
        ));
        if flush {
            // TODO: support flush
        }

        // close the duplicated dir fd
        unistd::close(fh as RawFd).expect(&format!(
            "release() failed to close the file handler {} of ino={}",
            fh, ino,
        ));
        reply.ok();
        INode::dec_open_count(inode);
        debug!(
            "release() successfully closed the file handler {} of ino={}",
            fh, ino,
        );
    }

    fn opendir(&mut self, req: &Request<'_>, ino: u64, flags: u32, reply: ReplyOpen) {
        debug!(
            "opendir(ino={}, flags={}, req={:?})",
            ino, flags, req.request,
        );

        let inode = self.cache.get(&ino).expect(&format!(
            "opendir() found fs is inconsistent, the i-node of ino={} should be in cache",
            ino,
        ));
        let oflags = util::parse_oflag(flags);
        let new_fd = inode.dup_fd(oflags);

        reply.opened(new_fd as u64, flags);
        debug!(
            "opendir() successfully duplicated the file handler of ino={}, new fd={}, flags: {:?}",
            ino, new_fd, oflags,
        );
    }

    fn releasedir(&mut self, req: &Request<'_>, ino: u64, fh: u64, flags: u32, reply: ReplyEmpty) {
        debug!(
            "releasedir(ino={}, fh={}, flags={}, req={:?})",
            ino, fh, flags, req.request,
        );
        let inode = self.cache.get(&ino).expect(&format!(
            "releasedir() found fs is inconsistent, the i-node of ino={} should be in cache",
            ino,
        ));
        // close the duplicated dir fd
        unistd::close(fh as RawFd).expect(&format!(
            "releasedir() failed to close the file handler {} of ino={}",
            fh, ino,
        ));
        reply.ok();
        INode::dec_open_count(inode);
        debug!(
            "releasedir() successfully closed the file handler {} of ino={}",
            fh, ino,
        );
    }

    fn read(&mut self, req: &Request, ino: u64, fh: u64, offset: i64, size: u32, reply: ReplyData) {
        debug!(
            "read(ino={}, fh={}, offset={}, size={}, req={:?})",
            ino, fh, offset, size, req.request,
        );

        let read_helper = |content: &Vec<u8>| {
            if (offset as usize) < content.len() {
                let read_data = if ((offset + size as i64) as usize) < content.len() {
                    &content[(offset as usize)..(offset + size as i64) as usize]
                } else {
                    &content[(offset as usize)..]
                };
                debug!(
                    "read() successfully from the file of ino={}, the read size is: {:?}",
                    ino,
                    read_data.len(),
                );
                reply.data(read_data);
            } else {
                debug!(
                    "read() offset={} is beyond the length of the file of ino={}",
                    offset, ino
                );
                reply.error(EINVAL);
            }
        };

        let inode = self.cache.get(&ino).expect(&format!(
            "read() found fs is inconsistent, the i-node of ino={} should be in cache",
            ino,
        ));
        inode.read_file(read_helper);
        // {
        //     // cache hit
        //     let file_data = INode::get_file_data(inode);

        //     if !file_data.is_empty() {
        //         debug!("read() cache hit when reading the data of ino={}", ino);
        //         read_helper(&*file_data, ino, offset, size, reply);
        //         return;
        //     }
        // }
        // {
        //     // cache miss
        //     debug!("read() cache missed when reading the data of ino={}", ino);
        //     // let inode = self.cache.get(&ino).expect(&format!(
        //     //     "read() found fs is inconsistent, the i-node of ino={} should be in cache",
        //     //     ino,
        //     // ));
        //     let fd = INode::get_file_fd(inode);
        //     let attr = INode::get_attr(inode);
        //     let mut file_data = INode::get_file_data_mut(inode);
        //     file_data.reserve(attr.size as usize);
        //     unsafe {
        //         file_data.set_len(file_data.capacity());
        //     }
        //     let res = unistd::read(fd.clone(), &mut *file_data);
        //     match res {
        //         Ok(s) => {
        //             unsafe {
        //                 file_data.set_len(s as usize);
        //             }
        //             read_helper(&file_data, ino, offset, size, reply); // TODO: zero file data copy
        //         }
        //         Err(e) => {
        //             reply.error(EIO);
        //             panic!(
        //                 "read() failed to read the file of ino={} from disk, the error is: {:?}",
        //                 ino, e,
        //             );
        //         }
        //     }
        // }
    }

    fn readdir(
        &mut self,
        req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        debug!(
            "readdir(ino={}, fh={}, offset={}, req={:?})",
            ino, fh, offset, req.request,
        );

        let readdir_helper = |data: &BTreeMap<OsString, DirEntry>| {
            let mut num_child_entries = 0;
            for (i, (child_name, child_entry)) in data.iter().enumerate().skip(offset as usize) {
                let child_ino = child_entry.ino;
                reply.add(
                    child_ino,
                    offset + i as i64 + 1, // i + 1 means the index of the next entry
                    util::convert_node_type(&child_entry.entry_type),
                    child_name,
                );
                num_child_entries += 1;
                debug!(
                    "readdir() found one child name={:?} ino={} offset={} entry={:?}
                        under the directory of ino={}",
                    child_name,
                    child_ino,
                    offset + i as i64 + 1,
                    child_entry,
                    ino,
                );
            }
            debug!(
                "readdir() successfully read {} children under the directory of ino={},
                    the reply is: {:?}",
                num_child_entries, ino, &reply,
            );
            reply.ok();
        };

        let inode = self.cache.get(&ino).expect(&format!(
            "readdir() found fs is inconsistent, the i-node of ino={} should be in cache",
            ino,
        ));
        inode.read_dir(readdir_helper);
        // {
        //     // cache hit
        //     let dir_data = INode::get_dir_data(inode);
        //     if !dir_data.is_empty() {
        //         debug!("readdir() cache hit when reading the data of ino={}", ino);
        //         readdir_helper(dir_data, &ino, offset, reply);
        //         return;
        //     }
        // }
        // {
        //     // cache miss
        //     debug!(
        //         "readdir() cache missed when reading the data of ino={}",
        //         ino,
        //     );
        //     let dir_fd = INode::get_dir_fd_mut(inode);
        //     match util::build_dir_data(&mut dir_fd) {
        //         Ok(dir_data) => {
        //             let old_data = INode::set_dir_data(inode, dir_data);
        //             debug_assert!(old_data.is_empty());
        //             readdir_helper(&dir_data, &ino, offset, reply);
        //         }
        //         Err(e) => panic!(
        //             "readdir() failed to read the directory of ino={},
        //                 the error is: {}",
        //             ino, e,
        //         ),
        //     }
        // }
    }

    fn lookup(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let child_name = OsString::from(name);
        debug!(
            "lookup(parent={}, name={:?}, req={:?})",
            parent, child_name, req.request,
        );

        let ino: u64;
        let child_type: FileType;
        {
            // lookup child ino and type first
            let parent_inode = self.cache.get(&parent).expect(&format!(
                "lookup() found fs is inconsistent,
                    the parent i-node of ino={} should be in cache",
                parent
            ));
            match parent_inode.get_entry(&child_name) {
                Some(child_entry) => {
                    ino = child_entry.ino;
                    child_type = util::convert_node_type(&child_entry.entry_type);
                }
                None => {
                    reply.error(ENOENT);
                    debug!(
                        "lookup() failed to find the file name={:?} under parent directory of ino={}",
                        child_name, parent
                    );
                    return;
                }
            }
        }

        let lookup_helper = |attr: &FileAttr| {
            let ttl = Duration::new(MY_TTL_SEC, 0);
            reply.entry(&ttl, &attr, MY_GENERATION);
            debug!(
                "lookup() successfully found the file name={:?} of ino={}
                    under parent ino={}, the attr is: {:?}",
                child_name, ino, parent, &attr,
            );
        };

        {
            // cache hit
            if let Some(inode) = self.cache.get(&ino) {
                debug!(
                    "lookup() cache hit when searching file of name: {:?} and ino={} under parent ino={}",
                    child_name, ino, parent,
                );
                inode.lookup_attr(lookup_helper);
                return;
            }
        }
        {
            // cache miss
            debug!(
                "lookup() cache missed when searching parent ino={}
                    and file name: {:?} of ino={}",
                parent, child_name, ino,
            );
            let parent_inode = self.cache.get(&parent).expect(&format!(
                "lookup() found fs is inconsistent, parent i-node of ino={} should be in cache",
                parent,
            ));
            let child_inode: INode;
            match child_type {
                FileType::Directory => {
                    child_inode = parent_inode.open_child_dir(&child_name);
                }
                FileType::RegularFile => {
                    let oflags = OFlag::O_RDONLY;
                    child_inode = parent_inode.open_child_file(&child_name, oflags);
                }
                _ => panic!("lookup() found unsupported file type: {:?}", child_type),
            };

            let child_ino = child_inode.get_ino();
            child_inode.lookup_attr(lookup_helper);
            self.cache.insert(child_ino, child_inode);
        }
    }

    fn forget(&mut self, req: &Request<'_>, ino: u64, nlookup: u64) {
        debug!(
            "forget(ino={}, nlookup={}, req={:?})",
            ino, nlookup, req.request,
        );
        let current_count: i64;
        {
            let inode = self.cache.get(&ino).expect(&format!(
                "forget() found fs is inconsistent, the i-node of ino={} should be in cache",
                ino,
            ));
            let previous_count = inode.dec_lookup_count_by(nlookup);
            current_count = inode.get_lookup_count();
            debug_assert!(current_count >= 0);
            debug_assert_eq!(previous_count - current_count, nlookup as i64); // assert thread-safe
            debug!(
                "forget() successfully reduced lookup count of ino={} from {} to {}",
                ino, previous_count, current_count,
            );
        }
        {
            if current_count == 0 {
                // TODO: support thread-safe
                if self.trash.contains(&ino) {
                    // deferred deletion
                    let deleted_inode = self.helper_unlink_node_by_ino(ino);
                    self.trash.remove(&ino);
                    debug_assert_eq!(deleted_inode.get_lookup_count(), 0);
                    debug!(
                        "forget() deferred deleted i-node of ino={}, the i-node is: {:?}",
                        ino, deleted_inode
                    );
                }
            }
        }
    }
    // Begin non-read functions

    /// called by the VFS to set attributes for a file. This method
    /// is called by chmod(2) and related system calls.
    fn setattr(
        &mut self,
        req: &Request,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<SystemTime>,
        mtime: Option<SystemTime>,
        fh: Option<u64>,
        crtime: Option<SystemTime>,
        chgtime: Option<SystemTime>,
        bkuptime: Option<SystemTime>,
        flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        debug!(
            "setattr(ino={}, mode={:?}, uid={:?}, gid={:?}, size={:?},
                atime={:?}, mtime={:?}, fh={:?}, crtime={:?}, chgtime={:?},
                bkuptime={:?}, flags={:?}, req={:?})",
            ino,
            mode,
            uid,
            gid,
            size,
            atime,
            mtime,
            fh,
            crtime,
            chgtime,
            bkuptime,
            flags,
            req.request,
        );

        let setattr_helper = |attr: &mut FileAttr| {
            let ttl = Duration::new(MY_TTL_SEC, 0);
            let ts = SystemTime::now();

            if let Some(b) = mode {
                attr.perm = util::parse_mode(b).bits();
                debug!("setattr set permission as: {}", attr.perm);

                let sflag = util::parse_sflag(b);
                let kind = util::convert_sflag(sflag);
                debug_assert_eq!(kind, attr.kind);
            }
            // no replace
            attr.uid = uid.unwrap_or(attr.uid);
            attr.gid = gid.unwrap_or(attr.gid);
            attr.size = size.unwrap_or(attr.size);
            attr.atime = atime.unwrap_or(attr.atime);
            attr.mtime = mtime.unwrap_or(attr.mtime);
            attr.crtime = crtime.unwrap_or(attr.crtime);
            attr.flags = flags.unwrap_or(attr.flags);

            if mode.is_some()
                || uid.is_some()
                || gid.is_some()
                || size.is_some()
                || atime.is_some()
                || mtime.is_some()
                || crtime.is_some()
                || chgtime.is_some()
                || bkuptime.is_some()
                || flags.is_some()
            {
                attr.ctime = ts; // update ctime, since meta data might change in setattr
                reply.attr(&ttl, attr);
                debug!(
                    "setattr successfully set the attribute of ino={}, the set attr is {:?}",
                    ino, attr,
                );
            } else {
                reply.error(ENODATA);
                error!(
                    "setattr found all the input attributes are empty for the file of ino={}",
                    ino,
                );
            }
        };

        let inode = self.cache.get_mut(&ino).expect(&format!(
            "setattr() found fs is inconsistent, the i-node of ino={} should be in cache",
            ino,
        ));
        inode.set_attr(setattr_helper);
        // {
        //     // cache hit
        //     if let Some(rc) = self.attr_cache.get(&ino) {
        //         let attr = &mut *rc.borrow_mut();
        //         debug!("setattr() cache hit when searching ino={}", ino);
        //         setattr_helper(
        //             &ino, mode, uid, gid, size, atime, mtime, crtime, chgtime, bkuptime, flags,
        //             attr, reply,
        //         );
        //         return; // attribute already updated by using mute borrow
        //     }
        // }
        // {
        //     // cache miss
        //     debug!("setattr() cache missed when searching ino={}", ino);
        //     let fd = {
        //         if fh.is_some() {
        //             fh.unwrap() as RawFd // safe to use unwrap() here
        //         } else {
        //             self.helper_get_fd_by_ino(&ino).expect(&format!(
        //                 "setattr() found fs is inconsistent,
        //                     node of ino={} should be opened before setattr()",
        //                 ino,
        //             ))
        //         }
        //     };
        //     let mut attr = util::read_attr(&fd).expect(&format!(
        //         "setattr() failed to read the attribute of ino={}",
        //         ino
        //     ));
        //     setattr_helper(
        //         &ino, mode, uid, gid, size, atime, mtime, crtime, chgtime, bkuptime, flags,
        //         &mut attr, reply,
        //     );
        //     self.attr_cache.insert(ino, attr);
        // }
        // TODO: write attribute to disk
    }

    fn mknod(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        mode: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let file_name = OsString::from(name);
        debug!(
            "mknod(parent={}, name={:?}, mode={}, rdev={}, req={:?})",
            parent, file_name, mode, rdev, req.request,
        );

        self.helper_create_node(parent, &file_name, mode, Type::File, reply);
    }

    fn unlink(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let file_name = OsString::from(name);
        debug!(
            "unlink(parent={}, name={:?}, req={:?}",
            parent, file_name, req.request,
        );
        self.helper_remove_node(parent, &file_name, Type::File, reply);
    }

    fn mkdir(&mut self, req: &Request, parent: u64, name: &OsStr, mode: u32, reply: ReplyEntry) {
        let dir_name = OsString::from(name);
        debug!(
            "mkdir(parent={}, name={:?}, mode={}, req={:?})",
            parent, dir_name, mode, req.request,
        );

        self.helper_create_node(parent, &dir_name, mode, Type::Directory, reply);
    }

    fn rmdir(&mut self, req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let dir_name = OsString::from(name);
        debug!(
            "rmdir(parent={}, name={:?}, req={:?})",
            parent, dir_name, req.request,
        );
        self.helper_remove_node(parent, &dir_name, Type::Directory, reply);
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        flags: u32,
        reply: ReplyWrite,
    ) {
        debug!(
            "write(ino={}, fh={}, offset={}, data-size={}, flags={})",
            // "write(ino={}, fh={}, offset={}, data-size={}, req={:?})",
            ino,
            fh,
            offset,
            data.len(),
            flags,
            // req.request,
        );

        let inode = self.cache.get_mut(&ino).expect(&format!(
            "write() found fs is inconsistent, the i-node of ino={} should be in cache",
            ino,
        ));
        let oflags = util::parse_oflag(flags);
        let written_size = inode.write_file(fh, offset, data, oflags);
        reply.written(written_size as u32);
        debug!(
            "write() successfully wrote {} byte data to file ino={} at offset={},
                the first at most 100 byte data are: {:?}",
            data.len(),
            ino,
            offset,
            if data.len() > 100 {
                &data[0..100]
            } else {
                data
            },
        );
        // {
        //     let file_data_ref = self.data_cache.get(&ino).expect(&format!(
        //         "write() found fs is inconsistent,
        //             file of ino={} should be opened before write()",
        //         ino
        //     ));
        //     match &mut *file_data_ref.borrow_mut() {
        //         FileData::Directory(_) => {
        //             reply.error(EISDIR);
        //             panic!(
        //                 "write() found fs is inconsistent,
        //                     the node type of ino={} should be a file not a directory",
        //                 ino,
        //             );
        //         }
        //         FileData::File(file_data) => {
        //             let size_after_write = offset as usize + data.len();
        //             if file_data.capacity() < size_after_write {
        //                 let before_cap = file_data.capacity();
        //                 let extra_space_size = size_after_write - file_data.capacity();
        //                 file_data.reserve(extra_space_size);
        //                 // TODO: handle OOM when reserving
        //                 // let result = file_data.try_reserve(extra_space_size);
        //                 // if result.is_err() {
        //                 //     warn!(
        //                 //         "write cannot reserve enough space, the space size needed is {} byte",
        //                 //         extra_space_size);
        //                 //     reply.error(ENOMEM);
        //                 //     return;
        //                 // }
        //                 debug!(
        //                     "write() enlarged the file data vector capacity from {} to {}",
        //                     before_cap,
        //                     file_data.capacity(),
        //                 );
        //             }
        //             match file_data.len().cmp(&(offset as usize)) {
        //                 cmp::Ordering::Greater => {
        //                     file_data.truncate(offset as usize);
        //                     debug!(
        //                         "write() truncated the file of ino={} to size={}",
        //                         ino, offset
        //                     );
        //                 }
        //                 cmp::Ordering::Less => {
        //                     let zero_padding_size = (offset as usize) - file_data.len();
        //                     let mut zero_padding_vec = vec![0u8; zero_padding_size];
        //                     file_data.append(&mut zero_padding_vec);
        //                 }
        //                 cmp::Ordering::Equal => (),
        //             }
        //             file_data.extend_from_slice(data);
        //             reply.written(data.len() as u32);
        //             // TODO: async write to disk
        //             let written_size = uio::pwrite(fh as RawFd, data, offset)
        //                 .expect("write() failed to write to disk");
        //             debug_assert_eq!(data.len(), written_size);

        //             // update the attribute of the written file
        //             let attr_ref = self.attr_cache.get(&ino).expect(&format!(
        //                 "write() found fs is inconsistent, no attribute found for ino={}",
        //                 ino,
        //             ));
        //             let attr = &mut *attr_ref.borrow_mut();
        //             attr.size = file_data.len() as u64;
        //             attr.flags = flags;
        //             let ts = SystemTime::now();
        //             attr.mtime = ts;

        //             debug!(
        //                 "write() successfully wrote {} byte data to file ino={} at offset={},
        //                  the attr is: {:?}, the first at most 100 byte data are: {:?}",
        //                 data.len(),
        //                 ino,
        //                 offset,
        //                 &attr,
        //                 if data.len() > 100 {
        //                     &data[0..100]
        //                 } else {
        //                     data
        //                 },
        //             );
        //         }
        //     }
        // }
    }
    /*
    /// Rename a file
    /// The filesystem must return -EINVAL for any unsupported or
    /// unknown flags. Currently the following flags are implemented:
    /// (1) RENAME_NOREPLACE: this flag indicates that if the target
    /// of the rename exists the rename should fail with -EEXIST
    /// instead of replacing the target.  The VFS already checks for
    /// existence, so for local filesystems the RENAME_NOREPLACE
    /// implementation is equivalent to plain rename.
    /// (2) RENAME_EXCHANGE: exchange source and target.  Both must
    /// exist; this is checked by the VFS.  Unlike plain rename,
    /// source and target may be of different type.
    fn rename(
        &mut self,
        req: &Request,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        newname: &OsStr,
        reply: ReplyEmpty,
    ) {
        let (old_name, new_name) = (OsString::from(name), OsString::from(newname));
        debug!(
            "rename(old parent={}, old name={:?}, new parent={}, new name={:?}, req={:?})",
            parent, old_name, new_parent, new_name, req.request,
        );

        let tree = &mut self
            .tree
            .write()
            .expect("rename cannot get the write lock of fs");

        // check the new parent has no entry with the same name as the rename file
        match tree.get(&new_parent) {
            Some(new_parent_node) => match &new_parent_node.data {
                FileData::Directory(new_parent_data) => {
                    if new_parent_data.contains_key(&new_name) {
                        reply.error(EEXIST); // RENAME_NOREPLACE
                        debug!(
                            "rename found the new parent directory of ino={} already has a child with name={:?}",
                            new_parent, new_name,
                        );
                        return;
                    }
                }
                FileData::File(_) => {
                    reply.error(ENOTDIR);
                    panic!(
                        "rename found fs is inconsistent, the node type of new parent ino={} should be a directory not a file",
                        parent,
                    );
                    // return;
                }
            },
            None => {
                reply.error(ENOENT);
                debug!(
                    "rename failed to find the i-node of new parent directory ino={}",
                    parent,
                );
                return;
            }
        };

        let rename_ino: u64;
        // check the old parent contains the rename file
        match tree.get(&parent) {
            Some(old_parent_node) => match &old_parent_node.data {
                FileData::Directory(old_parent_data) => match old_parent_data.get(&old_name) {
                    Some(old_ino) => rename_ino = old_ino.clone(),
                    None => {
                        reply.error(ENOENT);
                        debug!(
                                "rename cannot find the old file name={:?} under the old parent directory of ino={}",
                                old_name, parent,
                            );
                        return;
                    }
                },
                FileData::File(_) => {
                    reply.error(ENOTDIR);
                    panic!(
                        "rename found fs is inconsistent,
                         the node type of old parent ino={} should be a directory not a file",
                        parent,
                    );
                    // return;
                }
            },
            None => {
                reply.error(ENOENT);
                debug!(
                    "rename failed to find the i-node of old parent directory ino={}",
                    parent,
                );
                return;
            }
        }

        // check the i-node of rename file exists
        if !tree.contains_key(&rename_ino) {
            reply.error(ENOENT);
            panic!(
                "rename found fs is inconsistent, the file name={:?} of ino={}
                 found under the parent ino={}, but no i-node found for the file",
                old_name, rename_ino, parent,
            );
            // return;
        }

        // all checks passed, ready to rename, it's safe to use unwrap()
        if let FileData::Directory(old_parent_data) = &mut tree.get_mut(&parent).unwrap().data {
            // remove the inode of old file from old directory
            let rename_ino = old_parent_data.remove(&old_name).unwrap();
            if let FileData::Directory(new_parent_data) =
                &mut tree.get_mut(&new_parent).unwrap().data
            {
                // move from old parent directory to new parent
                new_parent_data.insert(new_name, rename_ino);

                let moved_file_info = tree.get_mut(&rename_ino).unwrap();
                // update the parent inode of the moved file
                moved_file_info.parent = new_parent;

                // change the ctime of the moved file
                let ts = SystemTime::now();
                moved_file_info.attr.ctime = ts;

                reply.ok();
                debug!(
                    "rename successfully moved the old file name={:?} of ino={} under old parent ino={}
                     to the new file name={:?} ino={} under new parent ino={}",
                    old_name, rename_ino, parent, newname, rename_ino, new_parent,
                );
                return;
            }
        }
        panic!("rename should never reach here");
    } */
}

fn main() {
    env_logger::init();

    let mountpoint = match env::args_os().nth(1) {
        Some(path) => path,
        None => {
            println!(
                "Usage: {} <MOUNTPOINT>",
                env::args().nth(0).unwrap(), // safe to use unwrap here
            );
            return;
        }
    };
    let options = [
        // "-d",
        //"-r",
        "-s",
        "-f",
        "-o",
        "debug",
        "-o",
        "fsname=fuse_rs_demo",
        "-o",
        "kill_on_unmount",
    ]
    .iter()
    .map(|o| o.as_ref())
    .collect::<Vec<&OsStr>>();

    let fs = MemoryFilesystem::new(&mountpoint);
    fuse::mount(fs, mountpoint, &options).expect("Couldn't mount filesystem");
}

#[cfg(test)]
mod test {
    #[test]
    fn test_tmp() {
        fn u64fn(u64ref: u64) {
            dbg!(u64ref);
        }
        let num: u64 = 100;
        let u64ref = &num;
        u64fn(u64ref.clone());
    }

    #[test]
    fn test_skip() {
        let v = vec![1, 2, 3, 4];
        for e in v.iter().skip(5) {
            dbg!(e);
        }
    }

    #[test]
    fn test_vec() {
        let mut v = vec![1, 2, 3, 4, 5];
        let cap = v.capacity();
        v.truncate(3);
        assert_eq!(v.len(), 3);
        assert_eq!(v.capacity(), cap);

        let mut v2 = vec![0; 3];
        v.append(&mut v2);
        assert_eq!(v.len(), 6);
        assert!(v2.is_empty());
    }

    #[test]
    fn test_map_swap() {
        use std::collections::{btree_map::Entry, BTreeMap};
        use std::ptr;
        use std::sync::RwLock;
        let mut map = BTreeMap::<String, Vec<u8>>::new();
        let (k1, k2, k3, k4) = ("A", "B", "C", "D");
        map.insert(k1.to_string(), vec![1]);
        map.insert(k2.to_string(), vec![2, 2]);
        map.insert(k3.to_string(), vec![3, 3]);
        map.insert(k4.to_string(), vec![4, 4, 4, 4]);

        let lock = RwLock::new(map);
        let mut map = lock.write().unwrap();

        let e1 = map.get_mut(k1).unwrap() as *mut _;
        let e2 = map.get_mut(k2).unwrap() as *mut _;
        // mem::swap(e1, e2);
        unsafe {
            ptr::swap(e1, e2);
        }
        dbg!(&map[k1]);
        dbg!(&map[k2]);

        let e3 = map.get_mut(k3).unwrap();
        e3.push(3);
        dbg!(&map[k3]);

        let k5 = "E";
        let e = map.entry(k5.to_string());
        if let Entry::Vacant(v) = e {
            v.insert(vec![5, 5, 5, 5, 5]);
        }
        dbg!(&map[k5]);
    }
    #[test]
    fn test_map_entry() {
        use std::collections::BTreeMap;
        use std::mem;
        let mut m1 = BTreeMap::<String, Vec<u8>>::new();
        let mut m2 = BTreeMap::<String, Vec<u8>>::new();
        let (k1, k2, k3, k4, k5) = ("A", "B", "C", "D", "E");
        m1.insert(k1.to_string(), vec![1]);
        m1.insert(k2.to_string(), vec![2, 2]);
        m2.insert(k3.to_string(), vec![3, 3, 3]);
        m2.insert(k4.to_string(), vec![4, 4, 4, 4]);

        let e1 = &mut m1.entry(k1.to_string());
        let e2 = &mut m2.entry(k5.to_string());
        mem::swap(e1, e2);

        dbg!(m1);
        dbg!(m2);
    }
}
