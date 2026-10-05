//! The in-memory file system served over NFS, plus the write-copy of client-written files.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::RwLock;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use nfsserve::nfs::{
    fattr3, fileid3, filename3, fsinfo3, ftype3, nfspath3, nfsstat3, nfstime3, sattr3, set_atime, set_gid3, set_mode3,
    set_mtime, set_size3, set_uid3, specdata3,
};
use nfsserve::nfs;
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use tracing::warn;

pub const ROOT_ID: fileid3 = 1;

/// read/write transfer size offered to clients (FSINFO max and preferred)
const IO_SIZE: u32 = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct Attr {
    pub mode: u32, // permission bits (and setuid/setgid/sticky)
    pub uid: u32,
    pub gid: u32,
    pub atime: nfstime3,
    pub mtime: nfstime3,
    pub ctime: nfstime3,
}

#[derive(Debug)]
pub enum Kind {
    Dir(BTreeMap<Vec<u8>, fileid3>),
    File(Vec<u8>),
    Symlink(Vec<u8>),
    /// character / block device, fifo, socket: (type, major, minor)
    Special(ftype3, u32, u32),
}

#[derive(Debug)]
pub struct Inode {
    pub attr: Attr,
    pub kind: Kind,
    pub nlink: u32,
    /// where this inode was first linked: its parent directory and name (for the write-copy path)
    pub parent: fileid3,
    pub name: Vec<u8>,
}

pub struct State {
    pub nodes: HashMap<fileid3, Inode>,
    next_id: fileid3,
}

pub fn now() -> nfstime3 {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    nfstime3 { seconds: d.as_secs() as u32, nseconds: d.subsec_nanos() }
}

impl State {
    pub fn new() -> State {
        let t = now();
        let root = Inode {
            attr: Attr { mode: 0o755, uid: 0, gid: 0, atime: t, mtime: t, ctime: t },
            kind: Kind::Dir(BTreeMap::new()),
            nlink: 2,
            parent: ROOT_ID,
            name: Vec::new(),
        };
        let mut nodes = HashMap::new();
        nodes.insert(ROOT_ID, root);
        State { nodes, next_id: ROOT_ID + 1 }
    }

    fn children(&self, dir: fileid3) -> Result<&BTreeMap<Vec<u8>, fileid3>, nfsstat3> {
        match &self.nodes.get(&dir).ok_or(nfsstat3::NFS3ERR_STALE)?.kind {
            Kind::Dir(c) => Ok(c),
            _ => Err(nfsstat3::NFS3ERR_NOTDIR),
        }
    }

    fn children_mut(&mut self, dir: fileid3) -> Result<&mut BTreeMap<Vec<u8>, fileid3>, nfsstat3> {
        match &mut self.nodes.get_mut(&dir).ok_or(nfsstat3::NFS3ERR_STALE)?.kind {
            Kind::Dir(c) => Ok(c),
            _ => Err(nfsstat3::NFS3ERR_NOTDIR),
        }
    }

    pub fn lookup(&self, dir: fileid3, name: &[u8]) -> Result<fileid3, nfsstat3> {
        if name == b"." {
            return Ok(dir);
        }
        if name == b".." {
            return Ok(self.nodes.get(&dir).ok_or(nfsstat3::NFS3ERR_STALE)?.parent);
        }
        self.children(dir)?.get(name).copied().ok_or(nfsstat3::NFS3ERR_NOENT)
    }

    /// Link a new inode into dir under name; an existing entry of that name is replaced
    /// (that is how later sources overlay earlier ones at load time).
    pub fn insert(&mut self, dir: fileid3, name: &[u8], attr: Attr, kind: Kind) -> Result<fileid3, nfsstat3> {
        if let Ok(old) = self.lookup(dir, name) {
            self.unlink(dir, name, old);
        }
        let id = self.next_id;
        self.next_id += 1;
        let is_dir = matches!(kind, Kind::Dir(_));
        self.nodes.insert(id, Inode { attr, kind, nlink: if is_dir { 2 } else { 1 }, parent: dir, name: name.to_vec() });
        self.children_mut(dir)?.insert(name.to_vec(), id);
        if is_dir {
            if let Some(p) = self.nodes.get_mut(&dir) {
                p.nlink += 1;
            }
        }
        Ok(id)
    }

    /// A hard link to an existing inode
    pub fn link(&mut self, dir: fileid3, name: &[u8], id: fileid3) -> Result<(), nfsstat3> {
        if let Ok(old) = self.lookup(dir, name) {
            self.unlink(dir, name, old);
        }
        self.children_mut(dir)?.insert(name.to_vec(), id);
        if let Some(n) = self.nodes.get_mut(&id) {
            n.nlink += 1;
        }
        Ok(())
    }

    fn unlink(&mut self, dir: fileid3, name: &[u8], id: fileid3) {
        if let Ok(c) = self.children_mut(dir) {
            c.remove(name);
        }
        let mut drop_ids = Vec::new();
        if let Some(n) = self.nodes.get_mut(&id) {
            if matches!(n.kind, Kind::Dir(_)) {
                drop_ids.push(id);
            } else {
                n.nlink = n.nlink.saturating_sub(1);
                if n.nlink == 0 {
                    drop_ids.push(id);
                }
            }
        }
        while let Some(d) = drop_ids.pop() {
            if let Some(n) = self.nodes.remove(&d) {
                if let Kind::Dir(c) = n.kind {
                    if let Some(p) = self.nodes.get_mut(&dir) {
                        p.nlink = p.nlink.saturating_sub(1);
                    }
                    drop_ids.extend(c.values().copied());
                }
            }
        }
    }

    /// Directory id for a path, creating missing directories as root-owned 0755
    pub fn mkdir_p(&mut self, path: &Path) -> Result<fileid3, nfsstat3> {
        let mut dir = ROOT_ID;
        for c in path.components() {
            let name = c.as_os_str().as_encoded_bytes();
            if name == b"/" || name.is_empty() {
                continue;
            }
            dir = match self.lookup(dir, name) {
                Ok(id) if matches!(self.nodes[&id].kind, Kind::Dir(_)) => id,
                _ => {
                    let t = now();
                    let attr = Attr { mode: 0o755, uid: 0, gid: 0, atime: t, mtime: t, ctime: t };
                    self.insert(dir, name, attr, Kind::Dir(BTreeMap::new()))?
                },
            };
        }
        Ok(dir)
    }

    /// The path of an inode relative to the root (its first link)
    pub fn rel_path(&self, mut id: fileid3) -> PathBuf {
        let mut parts = Vec::new();
        while id != ROOT_ID {
            match self.nodes.get(&id) {
                Some(n) => {
                    parts.push(String::from_utf8_lossy(&n.name).into_owned());
                    id = n.parent;
                },
                None => break,
            }
        }
        parts.iter().rev().collect()
    }

    pub fn fattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let n = self.nodes.get(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
        let (ftype, size, rdev) = match &n.kind {
            Kind::Dir(c) => (ftype3::NF3DIR, 4096 + 32 * c.len() as u64, specdata3::default()),
            Kind::File(d) => (ftype3::NF3REG, d.len() as u64, specdata3::default()),
            Kind::Symlink(t) => (ftype3::NF3LNK, t.len() as u64, specdata3::default()),
            Kind::Special(t, ma, mi) => (*t, 0, specdata3 { specdata1: *ma, specdata2: *mi }),
        };
        Ok(fattr3 {
            ftype,
            mode: n.attr.mode,
            nlink: n.nlink,
            uid: n.attr.uid,
            gid: n.attr.gid,
            size,
            used: size,
            rdev,
            fsid: 0,
            fileid: id,
            atime: n.attr.atime,
            mtime: n.attr.mtime,
            ctime: n.attr.ctime,
        })
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn bytes(&self) -> usize {
        self.nodes.values().map(|n| if let Kind::File(d) = &n.kind { d.len() } else { 0 }).sum()
    }
}

pub struct MemFs {
    pub state: RwLock<State>,
    /// client-written files are also written here (same relative path)
    pub write_copy: Option<PathBuf>,
}

impl MemFs {
    /// Where an inode is mirrored; None for a path that would not stay inside the write-copy directory
    fn copy_path(&self, st: &State, id: fileid3) -> Option<PathBuf> {
        let base = self.write_copy.as_ref()?;
        let rel = st.rel_path(id);
        if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
            warn!("write-copy: not mirroring {}", rel.display());
            return None;
        }
        Some(base.join(rel))
    }

    /// Mirror a write (or a truncation/extension when data is empty) into the write-copy directory
    fn copy_write(&self, path: Option<PathBuf>, offset: u64, data: &[u8], size: u64) {
        let Some(path) = path else { return };
        let res = (|| -> std::io::Result<()> {
            if let Some(p) = path.parent() {
                fs::create_dir_all(p)?;
            }
            let mut f = OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
            if !data.is_empty() {
                f.seek(SeekFrom::Start(offset))?;
                f.write_all(data)?;
            }
            if f.metadata()?.len() != size {
                f.set_len(size)?;
            }
            Ok(())
        })();
        if let Err(e) = res {
            warn!("write-copy {}: {}", path.display(), e);
        }
    }

    fn copy_rename(&self, from: Option<PathBuf>, to: Option<PathBuf>) {
        if let (Some(from), Some(to)) = (from, to) {
            if from.exists() {
                if let Some(p) = to.parent() {
                    let _ = fs::create_dir_all(p);
                }
                if let Err(e) = fs::rename(&from, &to) {
                    warn!("write-copy rename {} -> {}: {}", from.display(), to.display(), e);
                }
            }
        }
    }
}

/// A name a client may give a new directory entry: one path component, not "." or ".."
fn check_name(name: &[u8]) -> Result<(), nfsstat3> {
    match name {
        b"" => Err(nfsstat3::NFS3ERR_INVAL),
        b"." | b".." => Err(nfsstat3::NFS3ERR_EXIST),
        _ if name.contains(&b'/') || name.contains(&0) => Err(nfsstat3::NFS3ERR_INVAL),
        _ => Ok(()),
    }
}

fn apply_sattr(attr: &mut Attr, s: &sattr3) {
    if let set_mode3::mode(m) = s.mode {
        attr.mode = m & 0o7777;
    }
    if let set_uid3::uid(u) = s.uid {
        attr.uid = u;
    }
    if let set_gid3::gid(g) = s.gid {
        attr.gid = g;
    }
    match s.atime {
        set_atime::SET_TO_CLIENT_TIME(t) => attr.atime = t,
        set_atime::SET_TO_SERVER_TIME => attr.atime = now(),
        set_atime::DONT_CHANGE => {},
    }
    match s.mtime {
        set_mtime::SET_TO_CLIENT_TIME(t) => attr.mtime = t,
        set_mtime::SET_TO_SERVER_TIME => attr.mtime = now(),
        set_mtime::DONT_CHANGE => {},
    }
    attr.ctime = now();
}

fn new_attr(mode: u32) -> Attr {
    let t = now();
    Attr { mode, uid: 0, gid: 0, atime: t, mtime: t, ctime: t }
}

#[async_trait]
impl NFSFileSystem for MemFs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_ID
    }

    async fn fsinfo(&self, root_fileid: fileid3) -> Result<fsinfo3, nfsstat3> {
        let obj_attributes = match self.getattr(root_fileid).await {
            Ok(a) => nfs::post_op_attr::attributes(a),
            Err(_) => nfs::post_op_attr::Void,
        };
        Ok(fsinfo3 {
            obj_attributes,
            rtmax: IO_SIZE,
            rtpref: IO_SIZE,
            rtmult: 4096,
            wtmax: IO_SIZE,
            wtpref: IO_SIZE,
            wtmult: 4096,
            dtpref: IO_SIZE,
            maxfilesize: u32::MAX as u64,
            time_delta: nfstime3 { seconds: 0, nseconds: 1 },
            properties: nfs::FSF_SYMLINK | nfs::FSF_HOMOGENEOUS | nfs::FSF_CANSETTIME,
        })
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        self.state.read().unwrap().lookup(dirid, &filename.0)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        self.state.read().unwrap().fattr(id)
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let (copy, size) = {
            let mut st = self.state.write().unwrap();
            let n = st.nodes.get_mut(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
            apply_sattr(&mut n.attr, &setattr);
            let mut resized = None;
            if let set_size3::size(sz) = setattr.size {
                match &mut n.kind {
                    Kind::File(d) => {
                        d.resize(sz as usize, 0);
                        n.attr.mtime = now();
                        resized = Some(sz);
                    },
                    Kind::Dir(_) => return Err(nfsstat3::NFS3ERR_ISDIR),
                    _ => return Err(nfsstat3::NFS3ERR_INVAL),
                }
            }
            let copy = resized.and_then(|_| self.copy_path(&st, id));
            (copy, resized)
        };
        if let Some(sz) = size {
            self.copy_write(copy, 0, &[], sz);
        }
        self.state.read().unwrap().fattr(id)
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        let st = self.state.read().unwrap();
        match &st.nodes.get(&id).ok_or(nfsstat3::NFS3ERR_STALE)?.kind {
            Kind::File(d) => {
                let start = (offset as usize).min(d.len());
                let end = start.saturating_add(count as usize).min(d.len());
                Ok((d[start..end].to_vec(), end >= d.len()))
            },
            Kind::Dir(_) => Err(nfsstat3::NFS3ERR_ISDIR),
            _ => Err(nfsstat3::NFS3ERR_INVAL),
        }
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let (copy, size) = {
            let mut st = self.state.write().unwrap();
            let n = st.nodes.get_mut(&id).ok_or(nfsstat3::NFS3ERR_STALE)?;
            let size = match &mut n.kind {
                Kind::File(d) => {
                    let end = offset as usize + data.len();
                    if d.len() < end {
                        d.resize(end, 0);
                    }
                    d[offset as usize..end].copy_from_slice(data);
                    d.len() as u64
                },
                Kind::Dir(_) => return Err(nfsstat3::NFS3ERR_ISDIR),
                _ => return Err(nfsstat3::NFS3ERR_INVAL),
            };
            let t = now();
            n.attr.mtime = t;
            n.attr.ctime = t;
            (self.copy_path(&st, id), size)
        };
        self.copy_write(copy, offset, data, size);
        self.state.read().unwrap().fattr(id)
    }

    async fn create(&self, dirid: fileid3, filename: &filename3, attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        check_name(&filename.0)?;
        let (id, copy, size) = {
            let mut st = self.state.write().unwrap();
            // UNCHECKED create of an existing file: keep it, apply the attributes (e.g. truncation)
            let id = match st.lookup(dirid, &filename.0) {
                Ok(id) => {
                    if matches!(st.nodes[&id].kind, Kind::Dir(_)) {
                        return Err(nfsstat3::NFS3ERR_ISDIR);
                    }
                    id
                },
                Err(_) => st.insert(dirid, &filename.0, new_attr(0o644), Kind::File(Vec::new()))?,
            };
            let n = st.nodes.get_mut(&id).unwrap();
            apply_sattr(&mut n.attr, &attr);
            if let (set_size3::size(sz), Kind::File(d)) = (attr.size, &mut n.kind) {
                d.resize(sz as usize, 0);
            }
            let size = if let Kind::File(d) = &n.kind { d.len() as u64 } else { 0 };
            (id, self.copy_path(&st, id), size)
        };
        self.copy_write(copy, 0, &[], size);
        let fa = self.state.read().unwrap().fattr(id)?;
        Ok((id, fa))
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        check_name(&filename.0)?;
        let (id, copy) = {
            let mut st = self.state.write().unwrap();
            if st.lookup(dirid, &filename.0).is_ok() {
                return Err(nfsstat3::NFS3ERR_EXIST);
            }
            let id = st.insert(dirid, &filename.0, new_attr(0o644), Kind::File(Vec::new()))?;
            (id, self.copy_path(&st, id))
        };
        self.copy_write(copy, 0, &[], 0);
        Ok(id)
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        check_name(&dirname.0)?;
        let mut st = self.state.write().unwrap();
        if st.lookup(dirid, &dirname.0).is_ok() {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }
        let id = st.insert(dirid, &dirname.0, new_attr(0o755), Kind::Dir(BTreeMap::new()))?;
        let fa = st.fattr(id)?;
        Ok((id, fa))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let mut st = self.state.write().unwrap();
        let id = st.lookup(dirid, &filename.0)?;
        if let Kind::Dir(c) = &st.nodes[&id].kind {
            if !c.is_empty() {
                return Err(nfsstat3::NFS3ERR_NOTEMPTY);
            }
        }
        // the write-copy keeps its copy: logs survive rotation and deletion
        st.unlink(dirid, &filename.0, id);
        if let Some(d) = st.nodes.get_mut(&dirid) {
            d.attr.mtime = now();
        }
        Ok(())
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        check_name(&to_filename.0)?;
        let (from_copy, to_copy) = {
            let mut st = self.state.write().unwrap();
            let id = st.lookup(from_dirid, &from_filename.0)?;
            if let Ok(target) = st.lookup(to_dirid, &to_filename.0) {
                if target == id {
                    return Ok(());
                }
                if let Kind::Dir(c) = &st.nodes[&target].kind {
                    if !c.is_empty() {
                        return Err(nfsstat3::NFS3ERR_NOTEMPTY);
                    }
                }
                st.unlink(to_dirid, &to_filename.0, target);
            }
            let from_copy = self.copy_path(&st, id);
            st.children_mut(from_dirid)?.remove(&from_filename.0);
            st.children_mut(to_dirid)?.insert(to_filename.0.clone(), id);
            let is_dir = matches!(st.nodes[&id].kind, Kind::Dir(_));
            if is_dir && from_dirid != to_dirid {
                if let Some(d) = st.nodes.get_mut(&from_dirid) {
                    d.nlink = d.nlink.saturating_sub(1);
                }
                if let Some(d) = st.nodes.get_mut(&to_dirid) {
                    d.nlink += 1;
                }
            }
            let n = st.nodes.get_mut(&id).unwrap();
            n.parent = to_dirid;
            n.name = to_filename.0.clone();
            n.attr.ctime = now();
            (from_copy, self.copy_path(&st, id))
        };
        self.copy_rename(from_copy, to_copy);
        Ok(())
    }

    async fn readdir(&self, dirid: fileid3, start_after: fileid3, max_entries: usize) -> Result<ReadDirResult, nfsstat3> {
        let st = self.state.read().unwrap();
        let children = st.children(dirid)?;
        let mut iter = children.iter().peekable();
        if start_after != 0 {
            // resume after the entry with that id (names are unique, ids may repeat with hard links: first match)
            while let Some((_, id)) = iter.next() {
                if *id == start_after {
                    break;
                }
            }
        }
        let mut entries = Vec::new();
        while entries.len() < max_entries {
            match iter.next() {
                Some((name, id)) => entries.push(DirEntry { fileid: *id, name: name.clone().into(), attr: st.fattr(*id)? }),
                None => break,
            }
        }
        let end = iter.peek().is_none();
        Ok(ReadDirResult { entries, end })
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        check_name(&linkname.0)?;
        let mut st = self.state.write().unwrap();
        if st.lookup(dirid, &linkname.0).is_ok() {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }
        let mut a = new_attr(0o777);
        apply_sattr(&mut a, attr);
        let id = st.insert(dirid, &linkname.0, a, Kind::Symlink(symlink.0.clone()))?;
        let fa = st.fattr(id)?;
        Ok((id, fa))
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        match &self.state.read().unwrap().nodes.get(&id).ok_or(nfsstat3::NFS3ERR_STALE)?.kind {
            Kind::Symlink(t) => Ok(t.clone().into()),
            _ => Err(nfsstat3::NFS3ERR_INVAL),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs_with(names: &[&[u8]]) -> (MemFs, fileid3) {
        let mut st = State::new();
        let mut id = ROOT_ID;
        for (i, n) in names.iter().enumerate() {
            let kind = if i + 1 < names.len() { Kind::Dir(BTreeMap::new()) } else { Kind::File(Vec::new()) };
            id = st.insert(id, n, new_attr(0o644), kind).unwrap();
        }
        (MemFs { state: RwLock::new(st), write_copy: Some(PathBuf::from("/wc")) }, id)
    }

    #[test]
    fn client_names() {
        for bad in [&b""[..], b".", b"..", b"a/b", b"../x", b"/etc", b"a\0b"] {
            assert!(check_name(bad).is_err(), "{:?}", bad);
        }
        for good in [&b"a"[..], b"...", b".hidden", b"a b", b"a\\b"] {
            assert!(check_name(good).is_ok(), "{:?}", good);
        }
    }

    #[test]
    fn copy_path_stays_inside() {
        let (fs, id) = fs_with(&[b"var", b"log"]);
        assert_eq!(fs.copy_path(&fs.state.read().unwrap(), id), Some(PathBuf::from("/wc/var/log")));
        for bad in [&[&b"/etc"[..], b"passwd"][..], &[b"a", b"../../x"], &[b"a/../..", b"x"]] {
            let (fs, id) = fs_with(bad);
            assert_eq!(fs.copy_path(&fs.state.read().unwrap(), id), None, "{:?}", bad);
        }
    }
}
