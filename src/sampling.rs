//! Algorithm R with bounded storage and reproducible, non-cryptographic randomness.
//! Each input item has k/n inclusion probability under uniform random draws.
use anyhow::{Result, ensure};

pub struct Reservoir<T> {
    items: Vec<T>,
    capacity: usize,
    seen: u64,
    random: u64,
}
impl<T> Reservoir<T> {
    pub fn new(capacity: usize, seed: u64) -> Result<Self> {
        ensure!(capacity > 0, "reservoir size must be positive");
        Ok(Self {
            items: Vec::new(),
            capacity,
            seen: 0,
            random: seed,
        })
    }
    pub fn push(&mut self, item: T) -> Result<()> {
        self.seen = self
            .seen
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("sampling population overflow"))?;
        if self.items.len() < self.capacity {
            self.items.push(item);
        } else {
            let slot = self.below(self.seen);
            if slot < self.capacity as u64 {
                self.items[slot as usize] = item;
            }
        }
        Ok(())
    }
    pub fn seen(&self) -> u64 {
        self.seen
    }
    pub fn into_items(self) -> Vec<T> {
        self.items
    }
    fn next(&mut self) -> u64 {
        // SplitMix64: portable deterministic PRNG, not a security primitive.
        self.random = self.random.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.random;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, bound: u64) -> u64 {
        // Reject the short interval so modulo doesn't favor some indexes.
        let threshold = bound.wrapping_neg() % bound;
        loop {
            let value = self.next();
            if value >= threshold {
                return value % bound;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bounded_reproducible_and_no_duplicates() {
        let sample = || {
            let mut r = Reservoir::new(7, 42).unwrap();
            for i in 0..10000 {
                r.push(i).unwrap();
                assert!(r.items.len() <= 7);
            }
            assert_eq!(r.seen(), 10000);
            r.into_items()
        };
        let a = sample();
        assert_eq!(a, sample());
        assert_eq!(a.iter().collect::<std::collections::HashSet<_>>().len(), 7);
        let mut small = Reservoir::new(20, 0).unwrap();
        for i in 0..3 {
            small.push(i).unwrap();
        }
        assert_eq!(small.into_items(), vec![0, 1, 2]);
        assert!(Reservoir::<u8>::new(0, 0).is_err());
    }
    #[test]
    fn inclusion_frequency_has_no_first_or_last_item_bias() {
        let mut counts = [0; 10];
        for seed in 0..10000 {
            let mut r = Reservoir::new(3, seed).unwrap();
            for i in 0..10 {
                r.push(i).unwrap();
            }
            for i in r.into_items() {
                counts[i] += 1;
            }
        }
        for count in counts {
            assert!((2750..3250).contains(&count), "{counts:?}");
        }
    }
}
