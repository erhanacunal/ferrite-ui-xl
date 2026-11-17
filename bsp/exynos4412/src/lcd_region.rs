//! Damage regions: a few boxes instead of one bounding box, so the present
//! copies (and direct frames bring over) two far-apart changes, say the status
//! bar clock and a card's figure, without everything between them.

use core::sync::atomic::{AtomicUsize, Ordering};

/// A box: x0, y0, x1, y1 (exclusive); x0 >= x1 = empty.
pub type Rect = [usize; 4];
pub const EMPTY: Rect = [usize::MAX, usize::MAX, 0, 0];

/// Boxes a region holds at most (a present's copy is two regions together).
pub const MAX: usize = 8;

/// Boxes a region of drawn pixels keeps (`POST /api/compose?boxes=N`, 1..=4;
/// 1 = one bounding box). A present copies the boxes of two (this one's and
/// the previous one's), so up to twice as many.
pub static BOXES: AtomicUsize = AtomicUsize::new(4);

pub fn limit() -> usize {
    BOXES.load(Ordering::Relaxed).clamp(1, MAX / 2)
}

pub fn union(a: Rect, b: Rect) -> Rect {
    [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]
}

pub fn is_empty(r: &Rect) -> bool {
    r[0] >= r[2]
}

fn area(r: &Rect) -> usize {
    if is_empty(r) { 0 } else { (r[2] - r[0]) * (r[3] - r[1]) }
}

fn contains(a: &Rect, b: &Rect) -> bool {
    a[0] <= b[0] && a[1] <= b[1] && b[2] <= a[2] && b[3] <= a[3]
}

/// Pixels a merge of `a` and `b` adds to what they cover themselves (the
/// overlap counts as saved: it would be copied twice).
fn waste(a: &Rect, b: &Rect) -> isize {
    area(&union(*a, *b)) as isize - area(a) as isize - area(b) as isize
}

/// Up to [`MAX`] boxes, not empty; they may overlap (copied twice there).
#[derive(Clone, Copy)]
pub struct Region {
    n: usize,
    r: [Rect; MAX],
}

impl Region {
    pub const EMPTY: Region = Region { n: 0, r: [EMPTY; MAX] };

    pub fn is_empty(&self) -> bool {
        self.n == 0
    }

    pub fn boxes(&self) -> &[Rect] {
        &self.r[..self.n]
    }

    pub fn bounds(&self) -> Rect {
        self.boxes().iter().fold(EMPTY, |a, b| union(a, *b))
    }

    /// Pixels in its boxes (overlaps counted twice: what copying them costs).
    pub fn pixels(&self) -> usize {
        self.boxes().iter().map(area).sum()
    }

    /// Adds `b`, keeping at most `limit` boxes: merged into a box when that
    /// wastes nothing, else a box of its own, else the cheapest pair merges.
    pub fn add(&mut self, b: Rect, limit: usize) {
        if is_empty(&b) || self.r[..self.n].iter().any(|r| contains(r, &b)) {
            return;
        }
        let limit = limit.clamp(1, MAX);
        if self.n == MAX {
            self.merge_cheapest();
        }
        self.r[self.n] = b;
        self.n += 1;
        // Free merges (one box covers the pair as well as the two did),
        // repeated as a merge can make another one free.
        'again: loop {
            for i in 0..self.n {
                for j in i + 1..self.n {
                    if waste(&self.r[i], &self.r[j]) <= 0 {
                        self.merge(i, j);
                        continue 'again;
                    }
                }
            }
            break;
        }
        while self.n > limit {
            self.merge_cheapest();
        }
    }

    /// Merges the pair whose box wastes the least.
    fn merge_cheapest(&mut self) {
        let mut best = (isize::MAX, 0, 1);
        for i in 0..self.n {
            for j in i + 1..self.n {
                let w = waste(&self.r[i], &self.r[j]);
                if w < best.0 {
                    best = (w, i, j);
                }
            }
        }
        self.merge(best.1, best.2);
    }

    pub fn add_region(&mut self, o: &Region, limit: usize) {
        for b in o.boxes() {
            self.add(*b, limit);
        }
    }

    /// Box `j` into box `i` (`i < j`).
    fn merge(&mut self, i: usize, j: usize) {
        self.r[i] = union(self.r[i], self.r[j]);
        self.r[j] = self.r[self.n - 1];
        self.n -= 1;
    }

    /// Boxes from the top down (a copy publishes how far it got by rows).
    pub fn sort_by_top(&mut self) {
        self.r[..self.n].sort_unstable_by_key(|r| r[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn covers(g: &Region, b: &Rect) -> bool {
        (b[1]..b[3]).all(|y| (b[0]..b[2]).all(|x| g.boxes().iter().any(|r| r[0] <= x && x < r[2] && r[1] <= y && y < r[3])))
    }

    #[test]
    fn far_apart_stay_apart() {
        let mut g = Region::EMPTY;
        g.add([0, 0, 100, 40], 4);
        g.add([1100, 0, 1280, 40], 4);
        g.add([600, 700, 700, 760], 4);
        assert_eq!(g.boxes().len(), 3);
        assert_eq!(g.pixels(), 4000 + 7200 + 6000);
    }

    #[test]
    fn inside_and_adjacent_merge() {
        let mut g = Region::EMPTY;
        g.add([0, 0, 100, 100], 4);
        g.add([10, 10, 20, 20], 4);
        g.add([100, 0, 200, 100], 4);
        assert_eq!(g.boxes(), &[[0, 0, 200, 100]]);
    }

    #[test]
    fn limit_merges_cheapest_and_covers() {
        let mut g = Region::EMPTY;
        let boxes = [[0, 0, 10, 10], [500, 500, 510, 510], [20, 0, 30, 10], [1000, 0, 1010, 10], [0, 700, 10, 710]];
        for b in boxes {
            g.add(b, 3);
        }
        assert_eq!(g.boxes().len(), 3);
        for b in boxes {
            assert!(covers(&g, &b));
        }
        // The neighbours at the top left first; then the top row (a strip of 10 rows
        // wastes less than a column down to y 710).
        assert!(g.boxes().contains(&[0, 0, 1010, 10]));
        assert!(g.boxes().contains(&[500, 500, 510, 510]) && g.boxes().contains(&[0, 700, 10, 710]));
    }

    #[test]
    fn full_region_takes_more() {
        let mut g = Region::EMPTY;
        for i in 0..20 {
            g.add([i * 60, (i * 37) % 700, i * 60 + 10, (i * 37) % 700 + 10], MAX);
            assert!(g.boxes().len() <= MAX);
        }
        for i in 0..20 {
            assert!(covers(&g, &[i * 60, (i * 37) % 700, i * 60 + 10, (i * 37) % 700 + 10]));
        }
    }

    #[test]
    fn one_box_is_the_bounding_box() {
        let mut g = Region::EMPTY;
        g.add([0, 0, 10, 10], 1);
        g.add([500, 500, 510, 510], 1);
        assert_eq!(g.boxes(), &[[0, 0, 510, 510]]);
    }
}
