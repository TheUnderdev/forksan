//! `after` dependency resolution: turn a set of selected forks into an
//! execution forest.

use std::collections::HashMap;

/// A fork selected to fire at the current moment, as far as scheduling is
/// concerned.
pub trait Selected {
    fn name(&self) -> &str;
    /// The `after` dependency's fork name, if any.
    fn after(&self) -> Option<&str>;
}

/// Resolve the `after` dependencies among the selected forks into an
/// execution forest: `roots` run first (concurrently), and `children[i]` run
/// sequentially after fork `i` completes. A dependency that wasn't selected
/// this moment makes its dependent a root; cycles are broken by promoting
/// one member per cycle to a root (with a warning).
pub fn layer_dependencies<S: Selected>(selected: &[S]) -> (Vec<usize>, Vec<Vec<usize>>) {
    let n = selected.len();
    let name_to_idx: HashMap<&str, usize> = selected
        .iter()
        .enumerate()
        .map(|(i, s)| (s.name(), i))
        .collect();
    let mut dep: Vec<Option<usize>> = selected
        .iter()
        .enumerate()
        .map(|(i, s)| {
            s.after()
                .and_then(|a| name_to_idx.get(a).copied())
                .filter(|d| *d != i)
        })
        .collect();

    // Break cycles by cutting one edge per cycle: a node hanging OFF a cycle
    // keeps its edge (its dependency still runs — as part of the opened
    // chain), only a node that the dep chain leads back to loses its.
    loop {
        // Reach closure: a node runs iff it is a root or its dep runs.
        let mut reached: Vec<bool> = dep.iter().map(|d| d.is_none()).collect();
        loop {
            let mut changed = false;
            for i in 0..n {
                if !reached[i] {
                    if let Some(d) = dep[i] {
                        if reached[d] {
                            reached[i] = true;
                            changed = true;
                        }
                    }
                }
            }
            if !changed {
                break;
            }
        }
        if reached.iter().all(|r| *r) {
            break;
        }
        // Find an unreached node whose dep chain returns to itself and cut.
        let on_cycle = (0..n).find(|&i| {
            if reached[i] {
                return false;
            }
            let mut j = dep[i];
            for _ in 0..=n {
                match j {
                    Some(k) if k == i => return true,
                    Some(k) => j = dep[k],
                    None => return false,
                }
            }
            false
        });
        // Defensive fallback: an unreached component always contains a cycle.
        let cut = on_cycle.unwrap_or_else(|| (0..n).find(|&i| !reached[i]).unwrap());
        tracing::warn!(
            fork = %selected[cut].name(),
            "fork after cycle detected; running this fork first"
        );
        dep[cut] = None;
    }

    let mut children: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, d) in dep.iter().enumerate() {
        if let Some(d) = d {
            children[*d].push(i);
        }
    }
    let roots: Vec<usize> = (0..n).filter(|i| dep[*i].is_none()).collect();
    (roots, children)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct S(&'static str, Option<&'static str>);
    impl Selected for S {
        fn name(&self) -> &str {
            self.0
        }
        fn after(&self) -> Option<&str> {
            self.1
        }
    }

    #[test]
    fn chain_and_fanout() {
        // a <- b <- c, and d independent, e also after a.
        let sel = vec![
            S("a", None),
            S("b", Some("a")),
            S("c", Some("b")),
            S("d", None),
            S("e", Some("a")),
        ];
        let (roots, children) = layer_dependencies(&sel);
        assert_eq!(roots, vec![0, 3]);
        assert_eq!(children[0], vec![1, 4]);
        assert_eq!(children[1], vec![2]);
        assert!(children[2].is_empty());
        assert!(children[3].is_empty());
    }

    #[test]
    fn missing_dep_makes_root() {
        let sel = vec![S("a", Some("ghost")), S("b", None)];
        let (roots, children) = layer_dependencies(&sel);
        assert_eq!(roots, vec![0, 1]);
        assert!(children.iter().all(|c| c.is_empty()));
    }

    #[test]
    fn breaks_two_cycle() {
        // a <-> b, with c hanging off b.
        let sel = vec![S("a", Some("b")), S("b", Some("a")), S("c", Some("b"))];
        let (roots, children) = layer_dependencies(&sel);
        assert_eq!(roots.len(), 1);
        // One of a/b became root; c kept its edge to b.
        let all: Vec<usize> = {
            let mut v = roots.clone();
            for c in &children {
                v.extend(c);
            }
            v.sort_unstable();
            v
        };
        assert_eq!(all, vec![0, 1, 2]);
        // Whichever of a/b was cut, c keeps hanging off b.
        assert!(children[1].contains(&2));
    }

    #[test]
    fn breaks_self_and_three_cycle() {
        // Self-loops are filtered at parse time, but stay defensive here too.
        let sel = vec![
            S("x", Some("x")),
            S("a", Some("c")),
            S("b", Some("a")),
            S("c", Some("b")),
        ];
        let (roots, children) = layer_dependencies(&sel);
        // Every node ends up scheduled exactly once.
        let mut all: Vec<usize> = roots.clone();
        let mut stack = roots.clone();
        while let Some(i) = stack.pop() {
            for &c in &children[i] {
                all.push(c);
                stack.push(c);
            }
        }
        all.sort_unstable();
        assert_eq!(all, vec![0, 1, 2, 3]);
    }
}
