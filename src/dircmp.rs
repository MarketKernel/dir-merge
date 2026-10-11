//! Folder comparison engine. Scans both trees in a background thread, then
//! compares file pairs in parallel and streams results back to the UI.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtOrd};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel::{Receiver, Sender};
use globset::{Glob, GlobSet, GlobSetBuilder};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CompareMode {
    /// Byte-by-byte content comparison.
    #[default]
    Content,
    /// Size and modification time.
    Quick,
    /// Size only.
    Size,
}

impl CompareMode {
    pub const ALL: [CompareMode; 3] = [CompareMode::Content, CompareMode::Quick, CompareMode::Size];
    pub fn label(self) -> &'static str {
        match self {
            CompareMode::Content => "Full contents",
            CompareMode::Quick => "Size & date",
            CompareMode::Size => "Size only",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirOptions {
    pub mode: CompareMode,
    /// Comma separated glob patterns matched against file and folder names.
    pub exclude: String,
}

impl Default for DirOptions {
    fn default() -> Self {
        DirOptions {
            mode: CompareMode::Content,
            exclude: ".git, .svn, .hg, .DS_Store, Thumbs.db, desktop.ini".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pending,
    Identical,
    Different,
    LeftOnly,
    RightOnly,
    Error,
}

#[derive(Clone, Copy, Debug)]
pub struct Meta {
    pub size: u64,
    pub mtime: Option<SystemTime>,
    pub is_dir: bool,
}

#[derive(Clone, Debug)]
pub struct Node {
    pub name: String,
    pub parent: Option<u32>,
    pub depth: u16,
    pub children: Vec<u32>,
    pub left: Option<Meta>,
    pub right: Option<Meta>,
    pub is_dir: bool,
    pub status: Status,
    pub error: Option<String>,
    /// For directories: number of differing descendants (files only).
    pub diff_count: u32,
    /// Detached from the tree (deleted on both sides).
    pub removed: bool,
}

#[derive(Clone, Debug)]
pub struct DirTree {
    pub left_root: PathBuf,
    pub right_root: PathBuf,
    pub nodes: Vec<Node>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub identical: usize,
    pub different: usize,
    pub left_only: usize,
    pub right_only: usize,
    pub errors: usize,
    pub pending: usize,
}

impl DirTree {
    pub fn rel_path(&self, idx: u32) -> PathBuf {
        let mut parts = Vec::new();
        let mut cur = Some(idx);
        while let Some(i) = cur {
            let n = &self.nodes[i as usize];
            if n.parent.is_some() {
                parts.push(n.name.as_str());
            }
            cur = n.parent;
        }
        parts.iter().rev().collect()
    }

    pub fn left_path(&self, idx: u32) -> PathBuf {
        self.left_root.join(self.rel_path(idx))
    }

    pub fn right_path(&self, idx: u32) -> PathBuf {
        self.right_root.join(self.rel_path(idx))
    }

    /// Recompute directory statuses bottom-up from their files.
    pub fn aggregate(&mut self) {
        // Children always have larger indices than their parent.
        for i in (0..self.nodes.len()).rev() {
            if !self.nodes[i].is_dir {
                continue;
            }
            let (l, r) = (self.nodes[i].left.is_some(), self.nodes[i].right.is_some());
            if !(l && r) {
                let st = if l { Status::LeftOnly } else { Status::RightOnly };
                let cnt = self.subtree_files(i as u32);
                let n = &mut self.nodes[i];
                n.status = st;
                n.diff_count = cnt;
                continue;
            }
            if self.nodes[i].status == Status::Error && self.nodes[i].children.is_empty() {
                continue;
            }
            let mut pending = false;
            let mut differs = false;
            let mut count = 0u32;
            for &c in &self.nodes[i].children {
                let c = &self.nodes[c as usize];
                match c.status {
                    Status::Pending => pending = true,
                    Status::Identical => {}
                    _ => differs = true,
                }
                count += if c.is_dir { c.diff_count } else { (c.status != Status::Identical && c.status != Status::Pending) as u32 };
            }
            let n = &mut self.nodes[i];
            n.diff_count = count;
            n.status = if differs {
                Status::Different
            } else if pending {
                Status::Pending
            } else {
                Status::Identical
            };
        }
    }

    fn subtree_files(&self, idx: u32) -> u32 {
        let n = &self.nodes[idx as usize];
        if !n.is_dir {
            return 1;
        }
        n.children.iter().map(|&c| self.subtree_files(c)).sum()
    }

    pub fn stats(&self) -> Stats {
        let mut s = Stats::default();
        for n in self.nodes.iter().filter(|n| !n.is_dir && !n.removed) {
            match n.status {
                Status::Pending => s.pending += 1,
                Status::Identical => s.identical += 1,
                Status::Different => s.different += 1,
                Status::LeftOnly => s.left_only += 1,
                Status::RightOnly => s.right_only += 1,
                Status::Error => s.errors += 1,
            }
        }
        s
    }

    /// Find the node for a path relative to the roots.
    pub fn find(&self, rel: &Path) -> Option<u32> {
        let mut cur = 0u32;
        for comp in rel.components() {
            let name = comp.as_os_str().to_string_lossy();
            let key = key_of(&name);
            cur = *self.nodes[cur as usize].children.iter().find(|&&c| key_of(&self.nodes[c as usize].name) == key)?;
        }
        Some(cur)
    }

    /// Make a subtree mirror one side, after a copy operation.
    pub fn mark_copied(&mut self, idx: u32, to_right: bool) {
        // Copying creates missing destination folders up the chain.
        let mut p = self.nodes[idx as usize].parent;
        while let Some(pi) = p {
            let n = &mut self.nodes[pi as usize];
            let (src, dst) = if to_right { (n.left, &mut n.right) } else { (n.right, &mut n.left) };
            if dst.is_none() {
                *dst = src;
            }
            p = n.parent;
        }
        self.mark_copied_subtree(idx, to_right);
    }

    fn mark_copied_subtree(&mut self, idx: u32, to_right: bool) {
        let n = &mut self.nodes[idx as usize];
        let src = if to_right { n.left } else { n.right };
        // Items that exist only on the destination are left untouched by a copy.
        if src.is_none() {
            return;
        }
        if to_right {
            n.right = src;
        } else {
            n.left = src;
        }
        n.status = Status::Identical;
        n.error = None;
        let children = n.children.clone();
        for c in children {
            self.mark_copied_subtree(c, to_right);
        }
    }

    /// Mark one side of a subtree as deleted.
    pub fn mark_deleted(&mut self, idx: u32, left: bool) {
        let n = &mut self.nodes[idx as usize];
        if left {
            n.left = None;
        } else {
            n.right = None;
        }
        if n.left.is_none() && n.right.is_none() {
            self.remove_node(idx);
            return;
        }
        n.status = if left { Status::RightOnly } else { Status::LeftOnly };
        let children = n.children.clone();
        for c in children {
            self.mark_deleted(c, left);
        }
    }

    /// Detach a node from its parent (the arena slot stays, unreachable).
    fn remove_node(&mut self, idx: u32) {
        let children = self.nodes[idx as usize].children.clone();
        self.nodes[idx as usize].removed = true;
        for c in children {
            self.remove_node(c);
        }
        if let Some(p) = self.nodes[idx as usize].parent {
            self.nodes[p as usize].children.retain(|&c| c != idx);
        }
    }
}

pub enum Msg {
    Scanning { entries: usize },
    Tree(DirTree, usize),
    Results(Vec<(u32, Status, Option<String>)>),
    Done(Duration),
    Failed(String),
}

pub struct CompareJob {
    pub rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
}

impl CompareJob {
    pub fn cancel(&self) {
        self.cancel.store(true, AtOrd::Relaxed);
    }
}

impl Drop for CompareJob {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub fn start(left: PathBuf, right: PathBuf, opts: DirOptions, wake: impl Fn() + Send + Sync + 'static) -> CompareJob {
    let (tx, rx) = crossbeam_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let c2 = cancel.clone();
    std::thread::Builder::new()
        .name("dir-compare".into())
        .spawn(move || {
            let started = Instant::now();
            let res = run(&left, &right, &opts, &tx, &c2, &wake);
            match res {
                Ok(()) => tx.send(Msg::Done(started.elapsed())).ok(),
                Err(e) => tx.send(Msg::Failed(e)).ok(),
            };
            wake();
        })
        .expect("spawn compare thread");
    CompareJob { rx, cancel }
}

fn build_excludes(spec: &str) -> GlobSet {
    let mut b = GlobSetBuilder::new();
    for pat in spec.split([',', ';']).map(str::trim).filter(|p| !p.is_empty()) {
        if let Ok(g) = Glob::new(pat) {
            b.add(g);
        }
    }
    b.build().unwrap_or_else(|_| GlobSet::empty())
}

pub fn meta_of(path: &Path) -> io::Result<Meta> {
    let m = fs::metadata(path)?;
    Ok(Meta { size: if m.is_dir() { 0 } else { m.len() }, mtime: m.modified().ok(), is_dir: m.is_dir() })
}

/// A directory entry: name, metadata, and whether it is a symbolic link.
type Entry = (String, Meta, bool);

fn list_dir(dir: &Path, excludes: &GlobSet) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if excludes.is_match(&name) {
            continue;
        }
        let is_link = e.file_type().map(|t| t.is_symlink()).unwrap_or(false);
        // Follow links for metadata, but never recurse into linked folders (cycles).
        let meta = match meta_of(&e.path()) {
            Ok(m) => m,
            Err(_) => Meta { size: 0, mtime: None, is_dir: false },
        };
        out.push((name, meta, is_link));
    }
    Ok(out)
}

fn key_of(name: &str) -> String {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        name.to_lowercase()
    } else {
        name.to_string()
    }
}

/// Natural ordering: folders first, then names with numeric runs compared by value.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ia, mut ib) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ia.peek().copied(), ib.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ia.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    ia.next();
                }
                let mut nb = String::new();
                while let Some(c) = ib.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    ib.next();
                }
                let (ta, tb) = (na.trim_start_matches('0'), nb.trim_start_matches('0'));
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                ia.next();
                ib.next();
            }
        }
    }
}

struct Scanner<'a> {
    nodes: Vec<Node>,
    excludes: GlobSet,
    cancel: &'a AtomicBool,
    tx: &'a Sender<Msg>,
    wake: &'a (dyn Fn() + Send + Sync),
    last_report: Instant,
}

impl Scanner<'_> {
    fn scan(&mut self, idx: u32, left: Option<&Path>, right: Option<&Path>) {
        if self.cancel.load(AtOrd::Relaxed) {
            return;
        }
        let mut merged: BTreeMap<String, (Option<Entry>, Option<Entry>)> = BTreeMap::new();
        let mut error = None;
        for (side, dir) in [(0, left), (1, right)] {
            let Some(dir) = dir else { continue };
            match list_dir(dir, &self.excludes) {
                Ok(list) => {
                    for item in list {
                        let slot = merged.entry(key_of(&item.0)).or_default();
                        if side == 0 { slot.0 = Some(item) } else { slot.1 = Some(item) }
                    }
                }
                Err(e) => error = Some(format!("{}: {e}", dir.display())),
            }
        }
        if let Some(e) = error {
            let n = &mut self.nodes[idx as usize];
            n.status = Status::Error;
            n.error = Some(e);
        }
        let mut entries: Vec<_> = merged.into_values().collect();
        entries.sort_by(|a, b| {
            let da = a.0.as_ref().or(a.1.as_ref()).unwrap();
            let db = b.0.as_ref().or(b.1.as_ref()).unwrap();
            db.1.is_dir.cmp(&da.1.is_dir).then_with(|| natural_cmp(&da.0, &db.0))
        });
        let depth = self.nodes[idx as usize].depth + 1;
        for (l, r) in entries {
            let name = l.as_ref().or(r.as_ref()).unwrap().0.clone();
            let lm = l.as_ref().map(|x| x.1);
            let rm = r.as_ref().map(|x| x.1);
            let l_dir = lm.is_some_and(|m| m.is_dir);
            let r_dir = rm.is_some_and(|m| m.is_dir);
            let type_clash = lm.is_some() && rm.is_some() && l_dir != r_dir;
            let is_dir = (l_dir || r_dir) && !type_clash;
            let status = match (lm, rm) {
                _ if type_clash => Status::Different,
                (Some(_), None) => Status::LeftOnly,
                (None, Some(_)) => Status::RightOnly,
                _ => Status::Pending,
            };
            let child = self.nodes.len() as u32;
            self.nodes.push(Node {
                name: name.clone(),
                parent: Some(idx),
                depth,
                children: Vec::new(),
                left: lm,
                right: rm,
                is_dir,
                status,
                error: type_clash.then(|| "A file on one side, a folder on the other".to_string()),
                diff_count: 0,
                removed: false,
            });
            self.nodes[idx as usize].children.push(child);
            if self.last_report.elapsed() > Duration::from_millis(100) {
                self.last_report = Instant::now();
                self.tx.send(Msg::Scanning { entries: self.nodes.len() }).ok();
                (self.wake)();
            }
            let linked = l.as_ref().is_some_and(|x| x.2) || r.as_ref().is_some_and(|x| x.2);
            if is_dir && !linked {
                let lp = left.filter(|_| l_dir).map(|p| p.join(&l.as_ref().unwrap().0));
                let rp = right.filter(|_| r_dir).map(|p| p.join(&r.as_ref().unwrap().0));
                self.scan(child, lp.as_deref(), rp.as_deref());
            }
        }
    }
}

fn run(
    left: &Path,
    right: &Path,
    opts: &DirOptions,
    tx: &Sender<Msg>,
    cancel: &AtomicBool,
    wake: &(dyn Fn() + Send + Sync),
) -> Result<(), String> {
    let lm = meta_of(left).map_err(|e| format!("{}: {e}", left.display()))?;
    let rm = meta_of(right).map_err(|e| format!("{}: {e}", right.display()))?;
    if !lm.is_dir || !rm.is_dir {
        return Err("Both paths must be folders".into());
    }
    let root = Node {
        name: String::new(),
        parent: None,
        depth: 0,
        children: Vec::new(),
        left: Some(lm),
        right: Some(rm),
        is_dir: true,
        status: Status::Pending,
        error: None,
        diff_count: 0,
        removed: false,
    };
    let mut sc = Scanner {
        nodes: vec![root],
        excludes: build_excludes(&opts.exclude),
        cancel,
        tx,
        wake,
        last_report: Instant::now(),
    };
    sc.scan(0, Some(left), Some(right));
    if cancel.load(AtOrd::Relaxed) {
        return Ok(());
    }
    let mut tree = DirTree { left_root: left.to_path_buf(), right_root: right.to_path_buf(), nodes: sc.nodes };
    let pairs: Vec<(u32, PathBuf, PathBuf, Meta, Meta)> = tree
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, n)| !n.is_dir && n.status == Status::Pending)
        .map(|(i, n)| (i as u32, tree.left_path(i as u32), tree.right_path(i as u32), n.left.unwrap(), n.right.unwrap()))
        .collect();
    tree.aggregate();
    tx.send(Msg::Tree(tree, pairs.len())).ok();
    wake();

    let mode = opts.mode;
    pairs.par_chunks(64).for_each(|chunk| {
        if cancel.load(AtOrd::Relaxed) {
            return;
        }
        let results: Vec<_> = chunk
            .iter()
            .map(|(i, lp, rp, lm, rm)| match compare_files(lp, rp, lm, rm, mode, cancel) {
                Ok(true) => (*i, Status::Identical, None),
                Ok(false) => (*i, Status::Different, None),
                Err(e) => (*i, Status::Error, Some(e.to_string())),
            })
            .collect();
        tx.send(Msg::Results(results)).ok();
        wake();
    });
    Ok(())
}

fn mtime_eq(a: Option<SystemTime>, b: Option<SystemTime>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            let d = a.duration_since(b).or_else(|_| b.duration_since(a)).unwrap_or_default();
            d <= Duration::from_secs(2) // FAT has 2-second resolution
        }
        _ => false,
    }
}

pub fn compare_files(lp: &Path, rp: &Path, lm: &Meta, rm: &Meta, mode: CompareMode, cancel: &AtomicBool) -> io::Result<bool> {
    if lm.size != rm.size {
        return Ok(false);
    }
    match mode {
        CompareMode::Size => Ok(true),
        CompareMode::Quick => Ok(mtime_eq(lm.mtime, rm.mtime)),
        CompareMode::Content => {
            let mut fa = File::open(lp)?;
            let mut fb = File::open(rp)?;
            const N: usize = 256 * 1024;
            let mut ba = vec![0u8; N];
            let mut bb = vec![0u8; N];
            loop {
                if cancel.load(AtOrd::Relaxed) {
                    return Err(io::Error::other("cancelled"));
                }
                let na = read_full(&mut fa, &mut ba)?;
                let nb = read_full(&mut fb, &mut bb)?;
                if na != nb || ba[..na] != bb[..nb] {
                    return Ok(false);
                }
                if na < N {
                    return Ok(true);
                }
            }
        }
    }
}

fn read_full(f: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Recursively copy a file or folder, replacing the destination.
/// Files are written to a temporary name and renamed into place, so a reader of
/// the old file (e.g. a memory map) never sees it truncated.
pub fn copy_item(src: &Path, dst: &Path) -> io::Result<()> {
    let lm = fs::symlink_metadata(src)?;
    if lm.file_type().is_symlink() {
        #[cfg(unix)]
        {
            let target = fs::read_link(src)?;
            if fs::symlink_metadata(dst).is_ok() {
                if dst.is_dir() && !fs::symlink_metadata(dst)?.file_type().is_symlink() {
                    fs::remove_dir_all(dst)?;
                } else {
                    fs::remove_file(dst)?;
                }
            }
            return std::os::unix::fs::symlink(target, dst);
        }
        #[cfg(not(unix))]
        if fs::metadata(src)?.is_dir() {
            return Err(io::Error::other("linked folders are not copied"));
        }
    }
    let m = fs::metadata(src)?;
    if m.is_dir() {
        if dst.is_file() {
            fs::remove_file(dst)?;
        }
        fs::create_dir_all(dst)?;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_item(&e.path(), &dst.join(e.file_name()))?;
        }
    } else {
        if dst.is_dir() {
            fs::remove_dir_all(dst)?;
        }
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        let name = dst.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = dst.with_file_name(format!(".{name}.dirmerge-tmp"));
        let res = (|| {
            fs::copy(src, &tmp)?;
            // Keep the modification time so "size & date" comparisons agree.
            if let Ok(t) = m.modified() {
                File::options().write(true).open(&tmp).and_then(|f| f.set_modified(t)).ok();
            }
            fs::rename(&tmp, dst)
        })();
        if res.is_err() {
            fs::remove_file(&tmp).ok();
        }
        res?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run_sync(l: &Path, r: &Path) -> DirTree {
        let job = start(l.to_path_buf(), r.to_path_buf(), DirOptions::default(), || {});
        let mut tree = None;
        for msg in job.rx.iter() {
            match msg {
                Msg::Tree(t, _) => tree = Some(t),
                Msg::Results(rs) => {
                    let t = tree.as_mut().unwrap();
                    for (i, s, _) in rs {
                        t.nodes[i as usize].status = s;
                    }
                }
                Msg::Done(_) => break,
                Msg::Failed(e) => panic!("{e}"),
                Msg::Scanning { .. } => {}
            }
        }
        let mut t = tree.unwrap();
        t.aggregate();
        t
    }

    #[test]
    fn copy_out_of_one_sided_folder() {
        let base = std::env::temp_dir().join(format!("dm-copy-{}", std::process::id()));
        let (l, r) = (base.join("l"), base.join("r"));
        fs::create_dir_all(l.join("A")).unwrap();
        fs::create_dir_all(&r).unwrap();
        fs::write(l.join("A/f.txt"), "hello").unwrap();
        let mut t = run_sync(&l, &r);
        let f = t.find(Path::new("A/f.txt")).unwrap();
        copy_item(&t.left_path(f), &t.right_path(f)).unwrap();
        assert_eq!(fs::read_to_string(r.join("A/f.txt")).unwrap(), "hello");
        t.mark_copied(f, true);
        t.aggregate();
        let a = t.find(Path::new("A")).unwrap();
        assert_eq!(t.nodes[a as usize].status, Status::Identical);
        assert_eq!(t.stats().identical, 1);
        // Deleting the right copy makes it one-sided again; removing both drops it.
        t.mark_deleted(f, false);
        t.mark_deleted(f, true);
        t.aggregate();
        assert_eq!(t.stats().identical + t.stats().left_only + t.stats().right_only, 0);
        fs::remove_dir_all(base).ok();
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["file10", "file2", "File1", "a"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["a", "File1", "file2", "file10"]);
    }

    #[test]
    fn compare_tree() {
        let base = std::env::temp_dir().join(format!("dm-test-{}", std::process::id()));
        let (l, r) = (base.join("l"), base.join("r"));
        for d in [&l, &r] {
            fs::create_dir_all(d.join("sub")).unwrap();
            fs::write(d.join("same.txt"), "same").unwrap();
        }
        fs::write(l.join("sub/diff.txt"), "aaa").unwrap();
        fs::write(r.join("sub/diff.txt"), "aab").unwrap();
        fs::write(l.join("only_left.txt"), "x").unwrap();
        fs::create_dir_all(r.join(".git")).unwrap();

        let job = start(l.clone(), r.clone(), DirOptions::default(), || {});
        let mut tree = None;
        for msg in job.rx.iter() {
            match msg {
                Msg::Tree(t, _) => tree = Some(t),
                Msg::Results(rs) => {
                    let t = tree.as_mut().unwrap();
                    for (i, s, _) in rs {
                        t.nodes[i as usize].status = s;
                    }
                }
                Msg::Done(_) => break,
                Msg::Failed(e) => panic!("{e}"),
                Msg::Scanning { .. } => {}
            }
        }
        let mut t = tree.unwrap();
        t.aggregate();
        let s = t.stats();
        assert_eq!((s.identical, s.different, s.left_only, s.right_only), (1, 1, 1, 0));
        assert_eq!(t.nodes[0].status, Status::Different);
        assert_eq!(t.nodes[0].diff_count, 2);
        fs::remove_dir_all(base).ok();
    }
}
