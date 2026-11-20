//! Depth-limited raw-tree search for a window control by exact name.
/// Walk children in raw-view order, requiring both the window type and exact name.
pub(crate) fn window<N: Clone, E>(
    root: &N,
    name: &str,
    first: &impl Fn(&N) -> Result<Option<N>, E>,
    next: &impl Fn(&N) -> Result<Option<N>, E>,
    is_window: &impl Fn(&N) -> Result<bool, E>,
    node_name: &impl Fn(&N) -> Result<String, E>,
) -> Result<Option<N>, E> {
    search(root, name, 2, first, next, is_window, node_name)
}

fn search<N: Clone, E>(
    root: &N,
    name: &str,
    remaining: u32,
    first: &impl Fn(&N) -> Result<Option<N>, E>,
    next: &impl Fn(&N) -> Result<Option<N>, E>,
    is_window: &impl Fn(&N) -> Result<bool, E>,
    node_name: &impl Fn(&N) -> Result<String, E>,
) -> Result<Option<N>, E> {
    if remaining == 0 {
        return Ok(None);
    }
    let mut node = first(root)?;
    while let Some(current) = node {
        if node_name(&current)? == name && is_window(&current)? {
            return Ok(Some(current));
        }
        // The sibling is obtained before descending into this node.
        node = next(&current)?;
        if let Some(found) = search(&current, name, remaining - 1, first, next, is_window, node_name)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    struct Tree {
        children: Vec<Vec<usize>>,
        names: Vec<&'static str>,
        windows: Vec<bool>,
        calls: RefCell<Vec<String>>,
    }
    impl Tree {
        fn lookup(&self, name: &str) -> Result<Option<usize>, &'static str> {
            window(
                &0,
                name,
                &|n| {
                    self.calls.borrow_mut().push(format!("first:{n}"));
                    Ok(self.children[*n].first().copied())
                },
                &|n| {
                    self.calls.borrow_mut().push(format!("next:{n}"));
                    Ok(self
                        .children
                        .iter()
                        .find_map(|c| c.iter().position(|v| v == n).and_then(|i| c.get(i + 1).copied())))
                },
                &|n| {
                    self.calls.borrow_mut().push(format!("type:{n}"));
                    Ok(self.windows[*n])
                },
                &|n| {
                    self.calls.borrow_mut().push(format!("name:{n}"));
                    Ok(self.names[*n].into())
                },
            )
        }
    }
    #[test]
    fn window_control_rejects_same_named_non_window_and_finds_second_level() {
        let tree = Tree {
            children: vec![vec![1, 2], vec![], vec![3], vec![]],
            names: vec!["root", "FanaLab V1.66", "host", "FanaLab V1.66"],
            windows: vec![true, false, true, true],
            calls: RefCell::default(),
        };
        assert_eq!(tree.lookup("FanaLab V1.66"), Ok(Some(3)));
        let calls = tree.calls.borrow();
        assert!(calls.iter().position(|c| c == "name:1") < calls.iter().position(|c| c == "type:1"));
    }
    #[test]
    fn window_control_uses_raw_depth_first_order_and_stops_at_two_levels() {
        let tree = Tree {
            children: vec![vec![1, 4], vec![2], vec![3], vec![], vec![]],
            names: vec!["root", "container", "same", "deep", "same"],
            windows: vec![true, true, true, true, true],
            calls: RefCell::default(),
        };
        assert_eq!(tree.lookup("same"), Ok(Some(2)));
        assert_eq!(tree.lookup("deep"), Ok(None));
    }
}
