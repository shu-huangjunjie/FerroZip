//! Folder hierarchy reconstructed from flat archive entry names.

use std::collections::HashMap;
use std::time::SystemTime;

use fz_core::EntryInfo;

#[derive(Debug, Default)]
pub struct Node {
    pub name: String,
    /// Full path inside the archive, without trailing '/'. Empty for the root.
    pub path: String,
    pub parent: Option<usize>,
    pub dirs: Vec<usize>,
    pub files: Vec<usize>,
    /// Explicit directory entry, if the archive has one.
    pub entry: Option<usize>,
    pub size: u64,
    pub packed: u64,
    pub mtime: Option<SystemTime>,
    pub file_count: u64,
}

#[derive(Debug, Default)]
pub struct Tree {
    pub nodes: Vec<Node>,
    by_path: HashMap<String, usize>,
}

impl Tree {
    pub fn build(entries: &[EntryInfo]) -> Tree {
        let mut t = Tree { nodes: vec![Node::default()], by_path: HashMap::new() };
        t.by_path.insert(String::new(), 0);
        for (i, e) in entries.iter().enumerate() {
            if e.name.is_empty() {
                continue;
            }
            if e.is_dir {
                let n = t.ensure_dir(&e.name);
                t.nodes[n].entry = Some(i);
                if t.nodes[n].mtime.is_none() {
                    t.nodes[n].mtime = e.mtime;
                }
            } else {
                let parent = match e.name.rfind('/') {
                    Some(p) => t.ensure_dir(&e.name[..p]),
                    None => 0,
                };
                t.nodes[parent].files.push(i);
            }
        }
        // Aggregate sizes bottom-up: children always have higher indices.
        for n in (0..t.nodes.len()).rev() {
            let (mut size, mut packed, mut count) = (0u64, 0u64, 0u64);
            for &f in &t.nodes[n].files {
                size += entries[f].size;
                packed += entries[f].packed.unwrap_or(0);
                count += 1;
            }
            for &d in &t.nodes[n].dirs {
                size += t.nodes[d].size;
                packed += t.nodes[d].packed;
                count += t.nodes[d].file_count;
            }
            let node = &mut t.nodes[n];
            node.size = size;
            node.packed = packed;
            node.file_count = count;
        }
        t
    }

    fn ensure_dir(&mut self, path: &str) -> usize {
        if let Some(&n) = self.by_path.get(path) {
            return n;
        }
        let (parent, name) = match path.rfind('/') {
            Some(p) => (self.ensure_dir(&path[..p]), &path[p + 1..]),
            None => (0, path),
        };
        let n = self.nodes.len();
        self.nodes.push(Node { name: name.to_string(), path: path.to_string(), parent: Some(parent), ..Default::default() });
        self.nodes[parent].dirs.push(n);
        self.by_path.insert(path.to_string(), n);
        n
    }

    #[cfg(test)]
    pub fn find(&self, path: &str) -> Option<usize> {
        self.by_path.get(path.trim_matches('/')).copied()
    }

    /// Every entry index at or below `node`.
    pub fn collect(&self, node: usize, out: &mut Vec<usize>) {
        let n = &self.nodes[node];
        if let Some(e) = n.entry {
            out.push(e);
        }
        out.extend(&n.files);
        for &d in &n.dirs {
            self.collect(d, out);
        }
    }

    /// `a/b/` style prefix for a node (empty for root).
    pub fn prefix(&self, node: usize) -> String {
        let p = &self.nodes[node].path;
        if p.is_empty() { String::new() } else { format!("{p}/") }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str, is_dir: bool, size: u64) -> EntryInfo {
        EntryInfo { name: name.into(), is_dir, size, ..Default::default() }
    }

    #[test]
    fn implicit_and_explicit_dirs() {
        let entries = vec![e("a/b/c.txt", false, 5), e("a", true, 0), e("d.txt", false, 7)];
        let t = Tree::build(&entries);
        let a = t.find("a").unwrap();
        assert_eq!(t.nodes[a].entry, Some(1));
        let b = t.find("a/b").unwrap();
        assert_eq!(t.nodes[b].files, vec![0]);
        assert_eq!(t.nodes[0].size, 12);
        assert_eq!(t.nodes[0].file_count, 2);
        let mut all = Vec::new();
        t.collect(a, &mut all);
        all.sort();
        assert_eq!(all, vec![0, 1]);
    }
}
