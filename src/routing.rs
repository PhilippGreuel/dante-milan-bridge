//! Lock-freie Routing-Matrix: jeder Ausgang hat genau eine Quelle (oder keine).
//! Wird aus dem Echtzeit-Thread gelesen und aus der Web-UI geschrieben.

use std::sync::atomic::{AtomicI32, Ordering};

pub struct Router {
    outs: Vec<AtomicI32>,
    num_inputs: usize,
}

impl Router {
    pub fn new(num_inputs: usize, num_outputs: usize, initial: &[Option<u16>]) -> Self {
        let outs = (0..num_outputs)
            .map(|o| {
                let src = match initial.get(o) {
                    Some(Some(i)) if (*i as usize) < num_inputs => *i as i32,
                    Some(_) => -1,
                    // Default: 1:1-Routing
                    None if o < num_inputs => o as i32,
                    None => -1,
                };
                AtomicI32::new(src)
            })
            .collect();
        Self { outs, num_inputs }
    }

    pub fn num_inputs(&self) -> usize {
        self.num_inputs
    }
    pub fn num_outputs(&self) -> usize {
        self.outs.len()
    }

    #[inline]
    pub fn source(&self, out: usize) -> Option<usize> {
        let v = self.outs[out].load(Ordering::Relaxed);
        (v >= 0).then_some(v as usize)
    }

    pub fn set(&self, out: usize, src: Option<usize>) -> bool {
        if out >= self.outs.len() || src.is_some_and(|s| s >= self.num_inputs) {
            return false;
        }
        self.outs[out].store(src.map_or(-1, |s| s as i32), Ordering::Relaxed);
        true
    }

    pub fn snapshot(&self) -> Vec<Option<u16>> {
        (0..self.outs.len()).map(|o| self.source(o).map(|s| s as u16)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn default_is_identity_and_set_validates() {
        let r = Router::new(4, 6, &[]);
        assert_eq!(r.source(0), Some(0));
        assert_eq!(r.source(3), Some(3));
        assert_eq!(r.source(4), None);
        assert!(r.set(5, Some(1)));
        assert_eq!(r.source(5), Some(1));
        assert!(!r.set(5, Some(4)));
        assert!(r.set(0, None));
        assert_eq!(r.snapshot()[0], None);
    }
}
