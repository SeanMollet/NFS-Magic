# NFS-Magic

An NFSv3 server whose file system lives entirely in RAM. Built on [nfsserve](https://crates.io/crates/nfsserve).

At startup it loads squashfs images, directory trees and single files into memory, then serves them
read-write over NFSv3 + MOUNT on one TCP port (no rpcbind, no root, no kernel nfsd). Client writes only
change the RAM copy; with `--write-copy` the files a client creates or writes are mirrored to a host
directory (logs etc.). Everything else is thrown away when the server exits.

The use case: stand up an NFS root straight from a build output on an alternate port, boot a board from it,
collect its logs, discard the rest.

```
nfs-magic -s output/images/rootfs.squashfs -s output/system=/system \
    -x etc/fstab -f my-fstab=/etc/fstab -x 'etc/init.d/S40network' \
    --root-squash --write-copy /tmp/run1 --listen 0.0.0.0:11111
```

| option | |
|---|---|
| `-s, --source SRC[=DEST]` | squashfs image or directory loaded at DEST (default `/`); repeatable, later ones overlay earlier ones |
| `-f, --file SRC=DEST` | a single host file at DEST, added after all sources and regardless of `--exclude`; repeatable |
| `-x, --exclude GLOB` | path (relative to the export root) left out of the load; an excluded directory drops its subtree; repeatable |
| `-r, --root-squash` | imported files are owned by root:root |
| `-w, --write-copy DIR` | mirror client-created/written files here (same relative paths; deletions keep the copy) |
| `-l, --listen ADDR:PORT` | default `0.0.0.0:11111`; NFS and MOUNT share the port |
| `-e, --export NAME` | export (mount) path, default `/` |

Linux client / kernel NFS root:

```
mount -t nfs -o port=11111,mountport=11111,mountproto=tcp,vers=3,tcp,nolock HOST:/ /mnt
root=/dev/nfs rw nfsroot=HOST:/,port=11111,mountport=11111,mountproto=tcp,v3,tcp,nolock,rsize=65536,wsize=65536 ip=...
```

A kernel nfsroot defaults to `rsize=4096,wsize=4096`; give `rsize=`/`wsize=` to use the 64 KiB transfers the server offers.

On Windows (and other non-Unix hosts) a directory `--source` or `--file` carries no Unix metadata: files are
owned by root, modes are 0755 for directories and symlinks and 0644 for files (0444 if read-only), hard links
are loaded as separate copies, and there are no device nodes, fifos or sockets. Squashfs images keep their full
metadata on every host, so serve a root file system from an image there.

Build: `cargo build --release` → `target/release/nfs-magic`. Prebuilt binaries for Linux, macOS and Windows
(x86_64 and aarch64) are on the [Releases](https://github.com/SeanMollet/NFS-Magic/releases) page.
