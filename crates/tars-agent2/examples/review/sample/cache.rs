// A tiny fixed-capacity cache with obvious bugs.
use std::collections::HashMap;

struct Cache {
    cap: usize,
    map: HashMap<String, i32>,
}

impl Cache {
    /// Inserts into the cache.
    /// BUG: never enforces `cap` -> unbounded growth (capacity is ignored).
    fn put(&mut self, k: String, v: i32) {
        self.map.insert(k, v);
    }
    /// BUG: `len() - 1` underflows (usize) and panics when the cache is empty.
    fn last_index(&self) -> usize {
        self.map.len() - 1
    }
}

fn main() {
    let c = Cache { cap: 2, map: HashMap::new() };
    println!("{}", c.last_index());
}
