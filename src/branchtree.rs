//! Group branch names into folders by their `/` prefixes, like Sourcetree
//! (`feature/login`, `feature/ui` → folder `feature` with two branches).

#[derive(Debug, Default, PartialEq)]
pub struct Folder {
    /// Last path segment (`ui` for `feature/ui/`).
    pub name: String,
    /// Full prefix without trailing slash (`feature/ui`); empty for the root.
    pub path: String,
    pub folders: Vec<Folder>,
    pub leaves: Vec<Leaf>,
}

#[derive(Debug, PartialEq)]
pub struct Leaf {
    /// Last segment of the branch name, shown in the tree.
    pub label: String,
    /// Index into the list passed to [`build`].
    pub index: usize,
}

impl Folder {
    /// Number of branches in this folder and all sub-folders.
    pub fn count(&self) -> usize {
        self.leaves.len() + self.folders.iter().map(Folder::count).sum::<usize>()
    }

    /// Does this folder (or a sub-folder) hold the branch with this index?
    pub fn contains(&self, index: usize) -> bool {
        self.leaves.iter().any(|l| l.index == index)
            || self.folders.iter().any(|f| f.contains(index))
    }
}

/// Build the folder tree. Folders come first, then branches, each sorted case-insensitively.
pub fn build(names: &[&str]) -> Folder {
    let mut root = Folder::default();
    for (index, name) in names.iter().enumerate() {
        let parts: Vec<&str> = name.split('/').filter(|s| !s.is_empty()).collect();
        let Some((leaf, dirs)) = parts.split_last() else {
            continue;
        };
        let mut node = &mut root;
        for d in dirs {
            let pos = match node.folders.iter().position(|f| f.name == *d) {
                Some(p) => p,
                None => {
                    let path = if node.path.is_empty() {
                        d.to_string()
                    } else {
                        format!("{}/{d}", node.path)
                    };
                    node.folders.push(Folder {
                        name: d.to_string(),
                        path,
                        ..Default::default()
                    });
                    node.folders.len() - 1
                }
            };
            node = &mut node.folders[pos];
        }
        node.leaves.push(Leaf {
            label: leaf.to_string(),
            index,
        });
    }
    sort(&mut root);
    root
}

fn sort(f: &mut Folder) {
    f.folders.sort_by_key(|x| x.name.to_lowercase());
    f.leaves.sort_by_key(|x| x.label.to_lowercase());
    for sub in &mut f.folders {
        sort(sub);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_by_prefix() {
        let names = [
            "main",
            "feature/ui",
            "feature/login",
            "fix/a/deep",
            "develop",
        ];
        let t = build(&names);
        assert_eq!(
            t.leaves
                .iter()
                .map(|l| l.label.as_str())
                .collect::<Vec<_>>(),
            ["develop", "main"]
        );
        assert_eq!(
            t.folders
                .iter()
                .map(|f| f.name.as_str())
                .collect::<Vec<_>>(),
            ["feature", "fix"]
        );
        let feature = &t.folders[0];
        assert_eq!(feature.path, "feature");
        assert_eq!(
            feature
                .leaves
                .iter()
                .map(|l| (l.label.as_str(), l.index))
                .collect::<Vec<_>>(),
            [("login", 2), ("ui", 1)]
        );
        let fix = &t.folders[1];
        assert_eq!(fix.folders[0].path, "fix/a");
        assert_eq!(fix.folders[0].leaves[0].label, "deep");
        assert_eq!(t.count(), 5);
        assert_eq!(fix.count(), 1);
        assert!(fix.contains(3));
        assert!(!fix.contains(2));
    }

    #[test]
    fn flat_names_stay_flat() {
        let t = build(&["b", "A", "c"]);
        assert!(t.folders.is_empty());
        assert_eq!(
            t.leaves
                .iter()
                .map(|l| l.label.as_str())
                .collect::<Vec<_>>(),
            ["A", "b", "c"]
        );
    }
}
