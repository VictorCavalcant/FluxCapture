//! Bounded ring buffer of 48 kHz stereo frames.

/// Fixed-capacity FIFO of interleaved stereo frames. Never reallocates after construction.
#[derive(Clone, Debug)]
pub(crate) struct Fifo {
    buf: Vec<f32>,
    cap: usize,
    head: usize,
    len: usize,
}

impl Fifo {
    pub(crate) fn new(cap_frames: usize) -> Fifo {
        let cap = cap_frames.max(1);
        Fifo { buf: vec![0.0; 2 * cap], cap, head: 0, len: 0 }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn free(&self) -> usize {
        self.cap - self.len
    }

    pub(crate) fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Appends a frame; when full the oldest frame is dropped and `true` is returned.
    #[inline]
    pub(crate) fn push(&mut self, l: f32, r: f32) -> bool {
        let overflow = self.len == self.cap;
        if overflow {
            self.head += 1;
            if self.head == self.cap {
                self.head = 0;
            }
            self.len -= 1;
        }
        let mut t = self.head + self.len;
        if t >= self.cap {
            t -= self.cap;
        }
        let i = 2 * t;
        self.buf[i] = l;
        self.buf[i + 1] = r;
        self.len += 1;
        overflow
    }

    /// Appends interleaved stereo frames in bulk; when they do not fit the oldest frames are
    /// dropped and `true` is returned.
    pub(crate) fn extend(&mut self, frames: &[f32]) -> bool {
        let mut src = &frames[..frames.len() / 2 * 2];
        let mut n = src.len() / 2;
        if n > self.cap {
            src = &src[2 * (n - self.cap)..];
            n = self.cap;
        }
        let overflow = n > self.free();
        if overflow {
            self.drop_front(n - self.free());
        }
        let mut tail = self.head + self.len;
        if tail >= self.cap {
            tail -= self.cap;
        }
        let first = n.min(self.cap - tail);
        self.buf[2 * tail..2 * (tail + first)].copy_from_slice(&src[..2 * first]);
        self.buf[..2 * (n - first)].copy_from_slice(&src[2 * first..2 * n]);
        self.len += n;
        overflow || frames.len() / 2 > self.cap
    }

    /// Appends `n` silent frames (callers check `free()` first); returns whether data was dropped.
    pub(crate) fn push_silence(&mut self, n: usize) -> bool {
        let mut dropped = false;
        for _ in 0..n.min(self.cap) {
            dropped |= self.push(0.0, 0.0);
        }
        dropped
    }

    /// Drops the `n` oldest frames.
    pub(crate) fn drop_front(&mut self, n: usize) {
        let n = n.min(self.len);
        self.head = (self.head + n) % self.cap;
        self.len -= n;
    }

    /// Drops the `n` newest frames.
    pub(crate) fn drop_back(&mut self, n: usize) {
        self.len -= n.min(self.len);
    }

    /// Moves the `out.len() / 2` oldest frames into `out` (callers ensure `len()` suffices).
    pub(crate) fn pop_into(&mut self, out: &mut [f32]) {
        let n = (out.len() / 2).min(self.len);
        let first = n.min(self.cap - self.head);
        out[..2 * first].copy_from_slice(&self.buf[2 * self.head..2 * (self.head + first)]);
        let rest = n - first;
        out[2 * first..2 * n].copy_from_slice(&self.buf[..2 * rest]);
        self.drop_front(n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_and_overflows() {
        let mut f = Fifo::new(4);
        for i in 0..3 {
            assert!(!f.push(i as f32, -(i as f32)));
        }
        let mut out = [0.0; 4];
        f.pop_into(&mut out);
        assert_eq!(out, [0.0, -0.0, 1.0, -1.0]);
        for i in 3..7 {
            f.push(i as f32, 0.0);
        }
        assert!(f.push(7.0, 0.0));
        assert_eq!(f.len(), 4);
        let mut out = [0.0; 8];
        f.pop_into(&mut out);
        assert_eq!([out[0], out[2], out[4], out[6]], [4.0, 5.0, 6.0, 7.0]);
        f.push_silence(2);
        f.drop_back(1);
        assert_eq!(f.len(), 1);
        // Bulk append wrapping around, then overflowing.
        assert!(!f.extend(&[1.0, 1.0, 2.0, 2.0, 3.0, 3.0]));
        assert!(f.extend(&[4.0, 4.0, 5.0, 5.0]));
        let mut out = [0.0; 8];
        f.pop_into(&mut out);
        assert_eq!([out[0], out[2], out[4], out[6]], [2.0, 3.0, 4.0, 5.0]);
        assert!(f.extend(&[0.5; 20]));
        assert_eq!(f.len(), 4);
    }
}
