use std::collections::HashMap;

use fuser::INodeNo;

#[derive(Debug)]
pub(crate) struct InodeTable {
    next: u64,
    by_inode: HashMap<INodeNo, String>,
    by_path: HashMap<String, INodeNo>,
}

impl Default for InodeTable {
    fn default() -> Self {
        let mut by_inode = HashMap::new();
        let mut by_path = HashMap::new();
        by_inode.insert(INodeNo::ROOT, "/".into());
        by_path.insert("/".into(), INodeNo::ROOT);
        Self {
            next: 2,
            by_inode,
            by_path,
        }
    }
}

impl InodeTable {
    pub fn path(&self, inode: INodeNo) -> Option<String> {
        self.by_inode.get(&inode).cloned()
    }

    pub fn inode(&mut self, path: &str) -> INodeNo {
        if let Some(inode) = self.by_path.get(path) {
            return *inode;
        }
        let inode = INodeNo(self.next);
        self.next += 1;
        self.by_inode.insert(inode, path.to_owned());
        self.by_path.insert(path.to_owned(), inode);
        inode
    }

    pub fn rename(&mut self, from: &str, to: &str) {
        let prefix = format!("{from}/");
        let moves = self
            .by_path
            .iter()
            .filter(|(path, _)| *path == from || path.starts_with(&prefix))
            .map(|(path, inode)| (path.clone(), *inode))
            .collect::<Vec<_>>();
        for (old, inode) in moves {
            self.by_path.remove(&old);
            let suffix = old.strip_prefix(from).expect("selected prefix");
            let new = format!("{to}{suffix}");
            self.by_path.insert(new.clone(), inode);
            self.by_inode.insert(inode, new);
        }
    }

    pub fn remove(&mut self, path: &str) {
        let prefix = format!("{path}/");
        let removals = self
            .by_path
            .iter()
            .filter(|(candidate, _)| *candidate == path || candidate.starts_with(&prefix))
            .map(|(candidate, inode)| (candidate.clone(), *inode))
            .collect::<Vec<_>>();
        for (path, inode) in removals {
            self.by_path.remove(&path);
            self.by_inode.remove(&inode);
        }
    }
}

pub(crate) fn child(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else {
        format!("{parent}/{name}")
    }
}

pub(crate) fn parent(path: &str) -> &str {
    if path == "/" {
        return "/";
    }
    path.rsplit_once('/')
        .map(|(parent, _)| if parent.is_empty() { "/" } else { parent })
        .unwrap_or("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renames_entire_subtree() {
        let mut table = InodeTable::default();
        let dir = table.inode("/old");
        let file = table.inode("/old/file");
        table.rename("/old", "/new");
        assert_eq!(table.path(dir).as_deref(), Some("/new"));
        assert_eq!(table.path(file).as_deref(), Some("/new/file"));
    }
}
