//! Native macOS H.264 decoder and pooled BGRA display buffers.

use std::sync::{Arc, Mutex};

// Portable BT.709 reference kernels retained for color-regression checks.
mod annexb;
pub mod convert;
mod native;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("VideoToolbox status: {0}")]
    Native(i32),
    #[error("unsupported: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Packed BGRA pixels of one decoded frame, `width * 4` bytes per row.
#[derive(Debug)]
pub struct BgraFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Recycles the large buffers the zoomed CPU frames are written into. A
/// fresh 60 MB allocation per frame costs more in page faults than the
/// copy itself; a buffer that comes back from the display side is mapped
/// already.
#[derive(Clone, Default)]
pub struct PixelPool(Arc<Mutex<Vec<Vec<u8>>>>);

/// Buffers kept in a [`PixelPool`]: the one being written, one on screen,
/// one in transit.
const POOL_KEEP: usize = 3;

impl PixelPool {
    /// A buffer of exactly `len` bytes: a free one with the capacity when
    /// there is one, so its pages are mapped already, else a new one.
    fn take(&self, len: usize) -> Vec<u8> {
        let mut free = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut buf = match free.iter().position(|b| b.capacity() >= len) {
            Some(i) => free.swap_remove(i),
            None => Vec::with_capacity(len),
        };
        buf.resize(len, 0);
        buf
    }

    /// Keep `buf` for reuse. When the pool is full, a larger buffer
    /// replaces the smallest one, so the big zoomed buffers are the ones
    /// that stay when the zoom changes.
    fn give(&self, buf: Vec<u8>) {
        let mut free = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if free.len() < POOL_KEEP {
            free.push(buf);
            return;
        }
        if let Some(i) = (0..free.len()).min_by_key(|&i| free[i].capacity()) {
            if free[i].capacity() < buf.capacity() {
                free[i] = buf;
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

/// Pixels borrowed from a [`PixelPool`]; dropping them returns the buffer.
pub struct PooledPixels {
    buf: Vec<u8>,
    pool: PixelPool,
}

impl AsRef<[u8]> for PooledPixels {
    fn as_ref(&self) -> &[u8] {
        &self.buf
    }
}

impl Drop for PooledPixels {
    fn drop(&mut self) {
        self.pool.give(std::mem::take(&mut self.buf));
    }
}

/// A frame ready for display, possibly enlarged, in pooled memory.
pub struct PooledFrame {
    pub width: u32,
    pub height: u32,
    pub pixels: PooledPixels,
}

impl BgraFrame {
    /// Replicate every pixel into a `zoom` x `zoom` block, so the display
    /// side can show the frame at an integer scale without resampling. At
    /// zoom 1 the pixels move without a copy.
    pub fn zoomed(self, zoom: u32, pool: &PixelPool) -> PooledFrame {
        let pool = pool.clone();
        if zoom <= 1 {
            return PooledFrame {
                width: self.width,
                height: self.height,
                pixels: PooledPixels {
                    buf: self.pixels,
                    pool,
                },
            };
        }
        let z = zoom as usize;
        let (w, h) = (self.width as usize, self.height as usize);
        let src_stride = w * 4;
        let dst_stride = src_stride * z;
        let mut buf = pool.take(dst_stride * h * z);
        {
            use rayon::prelude::*;
            buf.par_chunks_exact_mut(dst_stride * z)
                .zip(self.pixels.par_chunks_exact(src_stride))
                .for_each(|(block, src_row)| {
                    let (first, rest) = block.split_at_mut(dst_stride);
                    for (dst, px) in first
                        .chunks_exact_mut(4 * z)
                        .zip(src_row.as_chunks::<4>().0)
                    {
                        for out in dst.as_chunks_mut::<4>().0 {
                            out.copy_from_slice(px);
                        }
                    }
                    for row in rest.chunks_exact_mut(dst_stride) {
                        row.copy_from_slice(first);
                    }
                });
        }
        PooledFrame {
            width: self.width * zoom,
            height: self.height * zoom,
            pixels: PooledPixels { buf, pool },
        }
    }
}

/// One native decoder per negotiated Single420 stream; no delayed output.
pub struct Decoder {
    stream: annexb::Stream,
    native: Option<native::Native>,
    size: (u32, u32),
}
impl Decoder {
    pub fn new(dual: bool, width: u32, height: u32) -> Result<Self> {
        if dual
            || width == 0
            || height == 0
            || width > 4096
            || height > 4096
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
        {
            return Err(Error::Unsupported(
                "only bounded, even Single420 streams supported".into(),
            ));
        }
        Ok(Self {
            stream: annexb::Stream::default(),
            native: None,
            size: (width, height),
        })
    }
    pub fn has_pending(&self) -> bool {
        false
    }
    pub fn flush(&mut self) -> Result<Option<BgraFrame>> {
        Ok(None)
    }
    pub fn decode(&mut self, main: &[u8], aux: &[u8]) -> Result<Option<BgraFrame>> {
        let result = self.decode_inner(main, aux);
        if result.is_err() {
            // Never retain broken references or parser/session format disagreement.
            self.native = None;
            self.stream.reset();
        }
        result.map(Some)
    }
    fn decode_inner(&mut self, main: &[u8], aux: &[u8]) -> Result<BgraFrame> {
        if !aux.is_empty() {
            return Err(Error::Unsupported("auxiliary stream not negotiated".into()));
        }
        let unit = self.stream.parse(main, self.size)?;
        if unit.changed || self.native.is_none() {
            self.native = Some(native::Native::new(&unit.parameters, self.size)?);
        }
        let len = (self.size.0 as usize)
            .checked_mul(self.size.1 as usize)
            .and_then(|n| n.checked_mul(4))
            .ok_or_else(|| Error::Unsupported("pixel size overflow".into()))?;
        let mut pixels = vec![0; len];
        self.native
            .as_mut()
            .expect("native session initialized")
            .decode(&unit.avcc, &mut pixels)?;
        Ok(BgraFrame {
            width: self.size.0,
            height: self.size.1,
            pixels,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn zoomed_replicates_each_pixel_into_a_block() {
        let pool = PixelPool::default();
        let frame = BgraFrame {
            width: 2,
            height: 1,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let z = frame.zoomed(2, &pool);
        assert_eq!((z.width, z.height), (4, 2));
        let row = [1, 2, 3, 4, 1, 2, 3, 4, 5, 6, 7, 8, 5, 6, 7, 8];
        assert_eq!(&z.pixels.as_ref()[..16], &row);
        assert_eq!(&z.pixels.as_ref()[16..], &row);
    }

    #[test]
    fn zoom_one_moves_the_pixels() {
        let pool = PixelPool::default();
        let frame = BgraFrame {
            width: 1,
            height: 1,
            pixels: vec![9, 9, 9, 9],
        };
        let z = frame.zoomed(1, &pool);
        assert_eq!(
            (z.width, z.height, z.pixels.as_ref()),
            (1, 1, &[9u8, 9, 9, 9][..])
        );
    }

    #[test]
    fn dropped_frames_return_their_buffers_to_the_pool() {
        let pool = PixelPool::default();
        let frame = BgraFrame {
            width: 2,
            height: 2,
            pixels: vec![0; 16],
        };
        let z = frame.zoomed(2, &pool);
        assert_eq!(pool.len(), 0);
        drop(z);
        assert_eq!(pool.len(), 1);
        let small = pool.take(16);
        assert_eq!(
            (small.len(), pool.len()),
            (16, 0),
            "a fitting buffer is reused"
        );
        drop(small);
        pool.give(vec![0; 8]);
        let big = pool.take(64);
        assert_eq!((big.len(), pool.len()), (64, 1), "a too-small buffer stays");
    }

    #[test]
    fn a_full_pool_keeps_the_largest_buffers() {
        let pool = PixelPool::default();
        for _ in 0..POOL_KEEP {
            pool.give(vec![0; 8]);
        }
        pool.give(vec![0; 64]);
        assert_eq!(pool.len(), POOL_KEEP);
        assert_eq!(
            pool.take(64).len(),
            64,
            "the large buffer replaced a small one"
        );
        pool.give(vec![0; 8]);
        pool.give(vec![0; 4]);
        assert!(
            pool.0.lock().unwrap().iter().all(|b| b.capacity() >= 8),
            "a smaller one is dropped"
        );
    }

    use super::*;
}
