//! Fixed-capacity ring buffer used for chart history series.

#[derive(Debug, Clone)]
pub struct RingBuffer<T> {
    data: Vec<T>,
    /// Index of the oldest element inside `data`.
    head: usize,
    len: usize,
}

impl<T: Clone + Default> RingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "capacity must be > 0");
        Self {
            data: vec![T::default(); capacity],
            head: 0,
            len: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Push a sample, overwriting the oldest when full.
    pub fn push(&mut self, value: T) {
        let idx = (self.head + self.len) % self.data.len();
        if self.len == self.data.len() {
            self.data[self.head] = value;
            self.head = (self.head + 1) % self.data.len();
        } else {
            self.data[idx] = value;
            self.len += 1;
        }
    }

    /// Element by chronological position (0 = oldest).
    pub fn get(&self, i: usize) -> Option<&T> {
        if i >= self.len {
            return None;
        }
        Some(&self.data[(self.head + i) % self.data.len()])
    }

    pub fn last(&self) -> Option<&T> {
        self.get(self.len.checked_sub(1)?)
    }

    pub fn iter(&self) -> impl Iterator<Item = &T> {
        (0..self.len).filter_map(move |i| self.get(i))
    }

    /// Last `n` samples in chronological order.
    pub fn tail(&self, n: usize) -> impl Iterator<Item = &T> {
        let start = self.len.saturating_sub(n);
        (start..self.len).filter_map(move |i| self.get(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_overwrites() {
        let mut rb = RingBuffer::new(3);
        for v in [10, 20, 30] {
            rb.push(v);
        }
        assert_eq!(rb.len(), 3);
        rb.push(40); // evicts 10
        let vals: Vec<i32> = rb.iter().copied().collect();
        assert_eq!(vals, vec![20, 30, 40]);
        rb.push(50);
        let vals: Vec<i32> = rb.iter().copied().collect();
        assert_eq!(vals, vec![30, 40, 50]);
    }

    #[test]
    fn partial_fill() {
        let mut rb = RingBuffer::new(5);
        rb.push(1);
        rb.push(2);
        let vals: Vec<i32> = rb.iter().copied().collect();
        assert_eq!(vals, vec![1, 2]);
        assert_eq!(rb.last(), Some(&2));
        let tail: Vec<i32> = rb.tail(5).copied().collect();
        assert_eq!(tail, vec![1, 2]);
    }

    #[test]
    fn tail_shorter_than_len() {
        let mut rb = RingBuffer::new(10);
        for v in 0..7 {
            rb.push(v);
        }
        let t: Vec<i32> = rb.tail(3).copied().collect();
        assert_eq!(t, vec![4, 5, 6]);
    }

    #[test]
    fn clear_resets() {
        let mut rb = RingBuffer::new(4);
        rb.push(9);
        rb.clear();
        assert!(rb.is_empty());
        assert_eq!(rb.get(0), None);
    }
}
