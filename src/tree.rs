//! A small shared tree-display layer.
//!
//! A [`DisplayTree`] is a value whose `Display` form is a one-line label and
//! which can name its children. [`IndentTree`] walks it and indents each level.
//! [`TreeLine`] is an owned tree for the few places that build labels on the
//! fly (syntax trees, statistics, statement descriptions) instead of modelling
//! a domain type as a tree.

use std::fmt;

/// A value that can be shown as an indented tree.
pub trait DisplayTree: fmt::Display {
    /// The children of this node, in display order.
    fn children(&self) -> Vec<&dyn DisplayTree>;
}

/// An indented view over a [`DisplayTree`].
pub struct IndentTree<'a> {
    root: &'a dyn DisplayTree,
}

impl<'a> IndentTree<'a> {
    /// An indented view of `root`.
    pub fn new(root: &'a dyn DisplayTree) -> Self {
        IndentTree { root }
    }
}

impl fmt::Display for IndentTree<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_node(f, self.root, 0)
    }
}

/// Write one node and its subtree, one line per node.
fn write_node(f: &mut fmt::Formatter<'_>, node: &dyn DisplayTree, depth: usize) -> fmt::Result {
    writeln!(f, "{}{}", "  ".repeat(depth), node)?;
    for child in node.children() {
        write_node(f, child, depth + 1)?;
    }
    Ok(())
}

/// An owned tree node whose label is built by its producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeLine {
    label: String,
    children: Vec<TreeLine>,
}

impl TreeLine {
    /// A leaf with `label`.
    pub fn new(label: impl Into<String>) -> Self {
        TreeLine {
            label: label.into(),
            children: Vec::new(),
        }
    }

    /// Add a child, returning `self` for chaining.
    pub fn with_child(mut self, child: TreeLine) -> Self {
        self.children.push(child);
        self
    }

    /// Add a child in place.
    pub fn push(&mut self, child: TreeLine) {
        self.children.push(child);
    }

    /// Copy any [`DisplayTree`] into an owned [`TreeLine`].
    pub fn from_tree(root: &dyn DisplayTree) -> Self {
        let mut line = TreeLine::new(root.to_string());
        for child in root.children() {
            line.push(TreeLine::from_tree(child));
        }
        line
    }

    /// Render this node and its children with the shared indentation.
    pub fn render(&self) -> String {
        IndentTree::new(self).to_string()
    }
}

impl fmt::Display for TreeLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

impl DisplayTree for TreeLine {
    fn children(&self) -> Vec<&dyn DisplayTree> {
        self.children
            .iter()
            .map(|child| child as &dyn DisplayTree)
            .collect()
    }
}

/// Build an owned tree from a pre-order stream of `(depth, label)` pairs.
///
/// The executor's `walk` and other flat traversals produce exactly this shape;
/// this helper keeps their front ends on the same indentation code.
pub fn tree_from_flat(items: Vec<(usize, String)>) -> Option<TreeLine> {
    let mut index = 0;
    if items.is_empty() {
        return None;
    }
    Some(build_flat(&items, &mut index, 0))
}

fn build_flat(items: &[(usize, String)], index: &mut usize, depth: usize) -> TreeLine {
    let mut node = TreeLine::new(items[*index].1.clone());
    *index += 1;
    while *index < items.len() && items[*index].0 > depth {
        node.push(build_flat(items, index, depth + 1));
    }
    node
}
