//! Populate the in-memory file system from squashfs images and directory trees.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use backhand::{FilesystemReader, InnerNode};
use globset::GlobSet;
use nfsserve::nfs::{fileid3, ftype3, nfstime3};

use crate::memfs::{Attr, Kind, ROOT_ID, State};

pub struct Opts<'a> {
    pub exclude: &'a GlobSet,
    pub root_squash: bool,
}

type Res<T> = Result<T, Box<dyn std::error::Error>>;

/// One --source: a squashfs image or a directory, placed at dest (relative to the export root)
pub fn load(st: &mut State, src: &Path, dest: &Path, o: &Opts) -> Res<()> {
    if fs::metadata(src)?.is_dir() {
        let base = put_dir_path(st, dest)?;
        let mut links = HashMap::new();
        load_dir(st, src, base, dest, o, &mut links)
    } else {
        load_squashfs(st, src, dest, o)
    }
}

/// One --file: a single host file (symlinks followed) placed at dest, replacing whatever is there
pub fn load_file(st: &mut State, src: &Path, dest: &Path, o: &Opts) -> Res<()> {
    let name = dest.file_name().ok_or_else(|| format!("{}: destination needs a file name", dest.display()))?;
    let parent = put_dir_path(st, dest.parent().unwrap_or(Path::new("")))?;
    let m = fs::metadata(src)?;
    if !m.is_file() {
        return Err(format!("{}: not a regular file", src.display()).into());
    }
    put(st, parent, name.as_encoded_bytes(), host_attr(o, &m), Kind::File(fs::read(src)?))?;
    Ok(())
}

fn excluded(o: &Opts, rel: &Path) -> bool {
    o.exclude.is_match(rel)
}

fn attr(o: &Opts, mode: u32, uid: u32, gid: u32, mtime: nfstime3) -> Attr {
    let (uid, gid) = if o.root_squash { (0, 0) } else { (uid, gid) };
    Attr { mode: mode & 0o7777, uid, gid, atime: mtime, mtime, ctime: mtime }
}

#[cfg(unix)]
fn host_attr(o: &Opts, m: &fs::Metadata) -> Attr {
    use std::os::unix::fs::MetadataExt;
    attr(o, m.mode(), m.uid(), m.gid(), nfstime3 { seconds: m.mtime() as u32, nseconds: m.mtime_nsec() as u32 })
}

/// No Unix owner or mode bits on this host: root-owned, permissions from the file type and read-only flag
#[cfg(not(unix))]
fn host_attr(o: &Opts, m: &fs::Metadata) -> Attr {
    let ft = m.file_type();
    let mode = if ft.is_dir() || ft.is_symlink() {
        0o755
    } else if m.permissions().readonly() {
        0o444
    } else {
        0o644
    };
    let d = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).unwrap_or_default();
    attr(o, mode, 0, 0, nfstime3 { seconds: d.as_secs() as u32, nseconds: d.subsec_nanos() })
}

/// Identity of a multiply-linked host file, so its other names become hard links
#[cfg(unix)]
fn link_key(m: &fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (m.nlink() > 1).then(|| (m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn link_key(_: &fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// Device, fifo or socket node
#[cfg(unix)]
fn host_special(m: &fs::Metadata) -> Option<Kind> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let ft = m.file_type();
    let (major, minor) = (((m.rdev() >> 8) & 0xfff) as u32, ((m.rdev() & 0xff) | ((m.rdev() >> 12) & 0xffffff00)) as u32);
    let t = if ft.is_char_device() {
        ftype3::NF3CHR
    } else if ft.is_block_device() {
        ftype3::NF3BLK
    } else if ft.is_fifo() {
        ftype3::NF3FIFO
    } else {
        ftype3::NF3SOCK
    };
    Some(Kind::Special(t, major, minor))
}

/// Nothing but files, directories and symlinks on this host
#[cfg(not(unix))]
fn host_special(_: &fs::Metadata) -> Option<Kind> {
    None
}

fn put_dir_path(st: &mut State, dest: &Path) -> Res<fileid3> {
    st.mkdir_p(dest).map_err(|e| format!("mkdir {}: {:?}", dest.display(), e).into())
}

/// A directory from a source: merges into an existing directory of that name (taking its attributes)
fn put_dir(st: &mut State, parent: fileid3, name: &[u8], a: Attr) -> Res<fileid3> {
    if let Ok(id) = st.lookup(parent, name) {
        if let Some(n) = st.nodes.get_mut(&id) {
            if matches!(n.kind, Kind::Dir(_)) {
                n.attr = a;
                return Ok(id);
            }
        }
    }
    Ok(st.insert(parent, name, a, Kind::Dir(BTreeMap::new())).map_err(|e| format!("{:?}", e))?)
}

fn put(st: &mut State, parent: fileid3, name: &[u8], a: Attr, kind: Kind) -> Res<fileid3> {
    Ok(st.insert(parent, name, a, kind).map_err(|e| format!("{:?}", e))?)
}

fn load_dir(
    st: &mut State,
    dir: &Path,
    id: fileid3,
    rel: &Path,
    o: &Opts,
    links: &mut HashMap<(u64, u64), fileid3>,
) -> Res<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name();
        let r = rel.join(&name);
        if excluded(o, &r) {
            continue;
        }
        let p = e.path();
        let m = fs::symlink_metadata(&p)?;
        let a = host_attr(o, &m);
        let n = name.as_encoded_bytes();
        let ft = m.file_type();
        if ft.is_dir() {
            let sub = put_dir(st, id, n, a)?;
            load_dir(st, &p, sub, &r, o, links)?;
            continue;
        }
        let key = link_key(&m);
        if let Some(k) = key {
            if let Some(&target) = links.get(&k) {
                if st.nodes.contains_key(&target) {
                    st.link(id, n, target).map_err(|e| format!("{:?}", e))?;
                    continue;
                }
            }
        }
        let kind = if ft.is_file() {
            Kind::File(fs::read(&p)?)
        } else if ft.is_symlink() {
            Kind::Symlink(fs::read_link(&p)?.as_os_str().as_encoded_bytes().to_vec())
        } else if let Some(kind) = host_special(&m) {
            kind
        } else {
            continue;
        };
        let new = put(st, id, n, a, kind)?;
        if let Some(k) = key {
            links.insert(k, new);
        }
    }
    Ok(())
}

fn load_squashfs(st: &mut State, src: &Path, dest: &Path, o: &Opts) -> Res<()> {
    let fsr = FilesystemReader::from_reader(BufReader::new(File::open(src)?))
        .map_err(|e| format!("{}: {}", src.display(), e))?;
    let base = put_dir_path(st, dest)?;
    // directory ids by squashfs path; parents come before their children
    let mut dirs: HashMap<PathBuf, fileid3> = HashMap::from([(PathBuf::new(), base)]);
    for node in fsr.files() {
        let full = node.fullpath.strip_prefix("/").unwrap_or(&node.fullpath);
        let h = &node.header;
        let a = attr(o, h.permissions as u32, h.uid, h.gid, nfstime3 { seconds: h.mtime, nseconds: 0 });
        if full.as_os_str().is_empty() {
            if dest.as_os_str().is_empty() || dest == Path::new("/") {
                st.nodes.get_mut(&ROOT_ID).unwrap().attr = a;
            }
            continue;
        }
        let rel = dest.join(full);
        let rel = rel.strip_prefix("/").unwrap_or(&rel);
        if excluded(o, rel) {
            continue;
        }
        // an excluded (or otherwise skipped) parent leaves the subtree out
        let Some(&parent) = dirs.get(full.parent().unwrap_or(Path::new(""))) else { continue };
        let name = full.file_name().unwrap().as_encoded_bytes();
        let kind = match &node.inner {
            InnerNode::Dir(_) => {
                let id = put_dir(st, parent, name, a)?;
                dirs.insert(full.to_path_buf(), id);
                continue;
            },
            InnerNode::File(f) => {
                let mut data = Vec::with_capacity(f.file_len());
                fsr.file(f).reader().read_to_end(&mut data)?;
                Kind::File(data)
            },
            InnerNode::Symlink(l) => Kind::Symlink(l.link.as_os_str().as_encoded_bytes().to_vec()),
            InnerNode::CharacterDevice(d) => special(ftype3::NF3CHR, d.device_number),
            InnerNode::BlockDevice(d) => special(ftype3::NF3BLK, d.device_number),
            InnerNode::NamedPipe => Kind::Special(ftype3::NF3FIFO, 0, 0),
            InnerNode::Socket => Kind::Special(ftype3::NF3SOCK, 0, 0),
        };
        put(st, parent, name, a, kind)?;
    }
    Ok(())
}

/// squashfs stores the kernel's new_encode_dev() form
fn special(t: ftype3, dev: u32) -> Kind {
    Kind::Special(t, (dev >> 8) & 0xfff, (dev & 0xff) | ((dev >> 12) & 0xfff00))
}
