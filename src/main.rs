//! nfs-magic: an NFSv3 server whose file system lives entirely in RAM.
//!
//! At startup it loads squashfs images and/or directory trees (later sources overlay earlier ones) and
//! single files (--file),
//! then serves them read-write over NFSv3 + MOUNT on one TCP port. Client writes change only the RAM
//! copy; with --write-copy they are also mirrored to a host directory (logs etc.), and everything else
//! is thrown away when the server exits.
//!
//! Camera netboot: root=/dev/nfs nfsroot=<host>:/,port=<P>,mountport=<P>,mountproto=tcp,v3,tcp,nolock,rsize=65536,wsize=65536

mod load;
mod memfs;

use std::path::PathBuf;
use std::sync::RwLock;
use std::time::Instant;

use clap::Parser;
use globset::{Glob, GlobSetBuilder};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use tracing::info;

use memfs::{MemFs, State};

#[derive(Parser)]
#[command(about = "NFSv3 server with an in-RAM file system loaded from squashfs images or directories")]
struct Args {
    /// SRC[=DEST]: squashfs image or directory to load at DEST (default /); repeatable, later ones overlay
    #[arg(short, long = "source", required = true, value_name = "SRC[=DEST]")]
    sources: Vec<String>,

    /// SRC=DEST: a single host file placed at DEST, after all sources and regardless of --exclude
    /// (e.g. --exclude etc/fstab --file my-fstab=/etc/fstab); repeatable
    #[arg(short, long = "file", value_name = "SRC=DEST")]
    files: Vec<String>,

    /// glob (relative to the export root, e.g. 'etc/init.d/S40network' or 'usr/share/man/**') left out of
    /// the load; an excluded directory drops its whole subtree; repeatable
    #[arg(short = 'x', long = "exclude", value_name = "GLOB")]
    excludes: Vec<String>,

    /// imported files are owned by root:root
    #[arg(short = 'r', long)]
    root_squash: bool,

    /// mirror files the client creates or writes to this directory (same relative paths)
    #[arg(short = 'w', long, value_name = "DIR")]
    write_copy: Option<PathBuf>,

    /// address to listen on (NFS and MOUNT share the port)
    #[arg(short, long, default_value = "0.0.0.0:11111")]
    listen: String,

    /// export name (mount path), default /
    #[arg(short, long, default_value = "/")]
    export: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,backhand=warn".into()))
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();

    let mut gb = GlobSetBuilder::new();
    for x in &args.excludes {
        gb.add(Glob::new(x.trim_start_matches('/'))?);
    }
    let exclude = gb.build()?;
    let opts = load::Opts { exclude: &exclude, root_squash: args.root_squash };

    let mut st = State::new();
    for s in &args.sources {
        let (src, dest) = match s.split_once('=') {
            Some((a, b)) => (a, b.trim_start_matches('/')),
            None => (s.as_str(), ""),
        };
        let t = Instant::now();
        load::load(&mut st, src.as_ref(), dest.as_ref(), &opts).map_err(|e| format!("{}: {}", src, e))?;
        info!("loaded {} at /{} in {:.2?}", src, dest, t.elapsed());
    }
    for f in &args.files {
        let (src, dest) = f.split_once('=').ok_or_else(|| format!("--file {}: expected SRC=DEST", f))?;
        let dest = dest.trim_start_matches('/');
        load::load_file(&mut st, src.as_ref(), dest.as_ref(), &opts).map_err(|e| format!("{}: {}", src, e))?;
        info!("added {} at /{}", src, dest);
    }
    info!("{} inodes, {:.1} MiB of file data", st.len(), st.bytes() as f64 / (1 << 20) as f64);

    if let Some(w) = &args.write_copy {
        std::fs::create_dir_all(w)?;
    }
    let fs = MemFs { state: RwLock::new(st), write_copy: args.write_copy };
    let mut listener = NFSTcpListener::bind(&args.listen, fs).await?;
    listener.with_export_name(&args.export);
    info!("serving {} on {} (port {})", args.export, args.listen, listener.get_listen_port());
    listener.handle_forever().await?;
    Ok(())
}
