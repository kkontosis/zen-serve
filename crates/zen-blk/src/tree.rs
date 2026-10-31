//! The verified tree of a volume (spec/blk.md §2, §5): the root record a
//! reader or writer holds, and the nodes it has checked against it.
//!
//! Every cached node is a node of the current tree: a reader's tree never
//! changes, and a writer replaces the nodes its commits change once they
//! apply. Nodes at level 2 and up (and the top node) are always kept; level-1
//! nodes are kept in an LRU of `cache_nodes`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};
use zen_core::blk::{
    FANOUT, Hash, Node, RootRecord, ZERO, depth, is_empty, leaf, node_hash, node_of, slot_of,
};

/// A node's position.
pub(crate) type Pos = (u8, u64);

/// A least-recently-used map of level-1 nodes.
struct Lru {
    cap: usize,
    tick: u64,
    map: HashMap<u64, (Arc<Node>, u64)>,
    order: BTreeMap<u64, u64>,
}

impl Lru {
    fn get(&mut self, j: u64) -> Option<Arc<Node>> {
        let (n, t) = self.map.get_mut(&j)?;
        self.order.remove(t);
        self.tick += 1;
        *t = self.tick;
        self.order.insert(self.tick, j);
        Some(n.clone())
    }

    fn put(&mut self, j: u64, n: Arc<Node>) {
        self.remove(j);
        self.tick += 1;
        self.map.insert(j, (n, self.tick));
        self.order.insert(self.tick, j);
        while self.map.len() > self.cap {
            let Some((_, old)) = self.order.pop_first() else {
                break;
            };
            self.map.remove(&old);
        }
    }

    fn remove(&mut self, j: u64) {
        if let Some((_, t)) = self.map.remove(&j) {
            self.order.remove(&t);
        }
    }
}

struct Cache {
    kept: HashMap<Pos, Arc<Node>>,
    lower: Lru,
}

/// What a set of blocks needs from the server.
pub(crate) struct Plan {
    /// The cached nodes on their paths.
    pub have: HashMap<Pos, Arc<Node>>,
    /// The nodes to fetch.
    pub need: Vec<Pos>,
    /// The blocks a cached zero slot shows absent.
    pub absent: HashSet<u64>,
}

/// The nodes a commit writes and deletes, and the new root.
pub(crate) struct Applied {
    pub root: Hash,
    pub set: Vec<(Pos, Arc<Node>)>,
    pub delete: Vec<Pos>,
}

/// A verified root record and its nodes.
pub(crate) struct Tree {
    pub record: RootRecord,
    /// `RH` of `record`.
    pub rh: Hash,
    pub depth: u8,
    cache: Mutex<Cache>,
}

impl Tree {
    pub fn new(record: RootRecord, rh: Hash, cache_nodes: usize) -> Self {
        Tree {
            depth: depth(record.blocks),
            record,
            rh,
            cache: Mutex::new(Cache {
                kept: HashMap::new(),
                lower: Lru {
                    cap: cache_nodes.max(1),
                    tick: 0,
                    map: HashMap::new(),
                    order: BTreeMap::new(),
                },
            }),
        }
    }

    fn cache(&self) -> std::sync::MutexGuard<'_, Cache> {
        self.cache.lock().expect("tree cache")
    }

    fn kept(&self, level: u8) -> bool {
        level >= 2 || level == self.depth
    }

    fn get(&self, c: &mut Cache, (l, j): Pos) -> Option<Arc<Node>> {
        if self.kept(l) {
            c.kept.get(&(l, j)).cloned()
        } else {
            c.lower.get(j)
        }
    }

    fn put(&self, c: &mut Cache, (l, j): Pos, n: Arc<Node>) {
        if self.kept(l) {
            c.kept.insert((l, j), n);
        } else {
            c.lower.put(j, n);
        }
    }

    fn drop_node(&self, c: &mut Cache, (l, j): Pos) {
        if self.kept(l) {
            c.kept.remove(&(l, j));
        } else {
            c.lower.remove(j);
        }
    }

    /// The paths of `blocks`: the cached nodes on them, the nodes to fetch,
    /// and the blocks known absent. Below an uncached node every slot is
    /// unknown, so every uncached node down to level 1 is fetched.
    pub fn plan(&self, blocks: &[u64]) -> Plan {
        let mut c = self.cache();
        let mut p = Plan {
            have: HashMap::new(),
            need: Vec::new(),
            absent: HashSet::new(),
        };
        let mut need = HashSet::new();
        for &i in blocks {
            let mut expected = Some(self.record.root);
            for l in (1..=self.depth).rev() {
                if expected == Some(ZERO) {
                    break;
                }
                let pos = (l, node_of(l, i));
                match p.have.get(&pos).cloned().or_else(|| self.get(&mut c, pos)) {
                    Some(n) => {
                        expected = expected.map(|_| n[slot_of(l, i)]);
                        p.have.insert(pos, n);
                    }
                    None => {
                        expected = None;
                        if need.insert(pos) {
                            p.need.push(pos);
                        }
                    }
                }
            }
            if expected == Some(ZERO) {
                p.absent.insert(i);
            }
        }
        p
    }

    /// Check block `i`'s path against the tree, top-down, with the nodes of
    /// `plan` and those fetched for it, and with `block` (`Some(stored
    /// bytes or None)`) its leaf too. Fetched nodes that check out are added
    /// to `plan.have` and `verified`.
    pub fn verify(
        &self,
        i: u64,
        plan: &mut Plan,
        fetched: &HashMap<Pos, Option<Node>>,
        block: Option<Option<&[u8]>>,
        verified: &mut Vec<(Pos, Arc<Node>)>,
    ) -> Result<(), String> {
        let mut expected = self.record.root;
        for l in (1..=self.depth).rev() {
            if expected == ZERO {
                break;
            }
            let pos = (l, node_of(l, i));
            let n = match plan.have.get(&pos) {
                Some(n) => n.clone(),
                None => {
                    let Some(Some(n)) = fetched.get(&pos) else {
                        return Err(format!("node ({l}, {}) is missing", pos.1));
                    };
                    if node_hash(l, pos.1, n) != expected {
                        return Err(format!("node ({l}, {}) doesn't match its parent", pos.1));
                    }
                    let n = Arc::new(*n);
                    plan.have.insert(pos, n.clone());
                    verified.push((pos, n.clone()));
                    n
                }
            };
            expected = n[slot_of(l, i)];
        }
        let Some(block) = block else { return Ok(()) };
        match block {
            Some(b) if leaf(i, b) == expected => Ok(()),
            Some(_) if expected == ZERO => Err(format!("block {i} is stored but not in the tree")),
            Some(_) => Err(format!("block {i} doesn't match its leaf")),
            None if expected == ZERO => Ok(()),
            None => Err(format!("block {i} is missing")),
        }
    }

    /// Block `i`'s leaf in this tree, given every node on its path in
    /// `have` (as [`Tree::verify`] leaves them).
    pub fn leaf_in(&self, i: u64, have: &HashMap<Pos, Arc<Node>>) -> Hash {
        let mut expected = self.record.root;
        for l in (1..=self.depth).rev() {
            if expected == ZERO {
                break;
            }
            match have.get(&(l, node_of(l, i))) {
                Some(n) => expected = n[slot_of(l, i)],
                // Not fetched: unknown, so not shown absent.
                None => return [0xff; 32],
            }
        }
        expected
    }

    /// Keep nodes that checked out.
    pub fn keep(&self, nodes: Vec<(Pos, Arc<Node>)>) {
        let mut c = self.cache();
        for (pos, n) in nodes {
            self.put(&mut c, pos, n);
        }
    }

    /// The tree with blocks set to new leaves (zeros: deleted), given every
    /// node on their paths in `have` (a node missing from it is empty).
    pub fn apply(&self, have: &HashMap<Pos, Arc<Node>>, leaves: &BTreeMap<u64, Hash>) -> Applied {
        let mut out = Applied {
            root: ZERO,
            set: Vec::new(),
            delete: Vec::new(),
        };
        let mut below: BTreeMap<u64, Hash> = leaves.clone();
        for l in 1..=self.depth {
            let mut touched: BTreeMap<u64, Node> = BTreeMap::new();
            for (c, h) in &below {
                let j = c / FANOUT as u64;
                let n = touched
                    .entry(j)
                    .or_insert_with(|| have.get(&(l, j)).map(|n| **n).unwrap_or([ZERO; FANOUT]));
                n[(*c % FANOUT as u64) as usize] = *h;
            }
            below = BTreeMap::new();
            for (j, n) in touched {
                below.insert(j, node_hash(l, j, &n));
                if is_empty(&n) {
                    if have.contains_key(&(l, j)) {
                        out.delete.push((l, j));
                    }
                } else {
                    out.set.push(((l, j), Arc::new(n)));
                }
            }
        }
        out.root = below.get(&0).copied().unwrap_or(ZERO);
        out
    }

    /// A commit of `a` applied: `record` is the new root.
    pub fn advance(&mut self, a: Applied, record: RootRecord, rh: Hash) {
        let mut c = self.cache.lock().expect("tree cache");
        for pos in a.delete {
            self.drop_node(&mut c, pos);
        }
        for (pos, n) in a.set {
            self.put(&mut c, pos, n);
        }
        drop(c);
        self.record = record;
        self.rh = rh;
    }
}
