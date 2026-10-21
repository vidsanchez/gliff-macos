//! Portable reference I420/YUV444 to BGRA conversion: BT.709 limited range,
//! fixed-point scalar row kernels spread over rayon. On ARM this preserves
//! the upstream scalar path. Native VideoToolbox output does not use these kernels.

use gliff_proto::chroma::Yuv444;
use rayon::prelude::*;

const FRAC: i32 = 13;
const HALF: i32 = 1 << (FRAC - 1);
const KY: i16 = 9539;
const KRV: i16 = 14686;
const KGU: i16 = -1747;
const KGV: i16 = -4366;
const KBU: i16 = 17305;

/// Unpack I420 (strides may exceed the width) to BGRA, each chroma sample
/// repeated over its 2x2 block. Width and height must be even.
pub fn i420_to_bgra(
    y: &[u8],
    u: &[u8],
    v: &[u8],
    strides: (usize, usize, usize),
    width: usize,
    height: usize,
) -> Vec<u8> {
    assert!(
        width.is_multiple_of(2) && height.is_multiple_of(2),
        "dimensions must be even"
    );
    let (sy, su, sv) = strides;
    let mut out = vec![0u8; width * height * 4];
    out.par_chunks_mut(width * 4)
        .enumerate()
        .for_each(|(row, line)| {
            let cw = width / 2;
            row_bgra(
                &y[row * sy..][..width],
                &u[row / 2 * su..][..cw],
                &v[row / 2 * sv..][..cw],
                line,
                true,
            );
        });
    out
}

/// Convert planar 4:4:4 back to BGRA.
pub fn yuv444_to_bgra(src: &Yuv444) -> Vec<u8> {
    let w = src.width;
    let mut out = vec![0u8; w * src.height * 4];
    out.par_chunks_mut(w * 4)
        .enumerate()
        .for_each(|(row, line)| {
            let at = row * w..row * w + w;
            row_bgra(
                &src.y[at.clone()],
                &src.u[at.clone()],
                &src.v[at],
                line,
                false,
            );
        });
    out
}

fn row_bgra(y: &[u8], u: &[u8], v: &[u8], out: &mut [u8], upsample: bool) {
    for (i, (px, y)) in out.as_chunks_mut::<4>().0.iter_mut().zip(y).enumerate() {
        let c = if upsample { i / 2 } else { i };
        let yy = *y as i32 - 16;
        let cb = u[c] as i32 - 128;
        let cr = v[c] as i32 - 128;
        let r = (KY as i32 * yy + KRV as i32 * cr + HALF) >> FRAC;
        let g = (KY as i32 * yy + KGU as i32 * cb + KGV as i32 * cr + HALF) >> FRAC;
        let b = (KY as i32 * yy + KBU as i32 * cb + HALF) >> FRAC;
        px.copy_from_slice(&[clamp(b), clamp(g), clamp(r), 255]);
    }
}

fn clamp(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use gliff_proto::chroma::nv12_to_yuv444;
    use gliff_proto::color;

    struct Rng(u32);

    impl Rng {
        fn next(&mut self) -> u8 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            (self.0 >> 8) as u8
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next()).collect()
        }
    }

    const SIZES: [(usize, usize); 7] = [
        (2, 2),
        (6, 4),
        (16, 2),
        (34, 6),
        (66, 4),
        (100, 2),
        (640, 4),
    ];

    fn max_diff(a: &[u8], b: &[u8]) -> u8 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn i420_to_bgra_matches_the_reference_within_one() {
        let mut rng = Rng(17);
        for (w, h) in SIZES {
            let (sy, sc) = (w + 6, w / 2 + 2);
            let y = rng.bytes(sy * h);
            let u = rng.bytes(sc * h / 2);
            let v = rng.bytes(sc * h / 2);
            let got = i420_to_bgra(&y, &u, &v, (sy, sc, sc), w, h);
            let mut nv12 = gliff_proto::chroma::Nv12::new(w, h);
            for row in 0..h {
                nv12.y[row * w..row * w + w].copy_from_slice(&y[row * sy..row * sy + w]);
            }
            for row in 0..h / 2 {
                for col in 0..w / 2 {
                    nv12.uv[row * w + 2 * col] = u[row * sc + col];
                    nv12.uv[row * w + 2 * col + 1] = v[row * sc + col];
                }
            }
            let want = color::yuv444_to_bgra(&nv12_to_yuv444(&nv12));
            assert!(max_diff(&got, &want) <= 1, "at {w}x{h}");
        }
    }

    #[test]
    fn yuv444_to_bgra_matches_the_reference_within_one() {
        let mut rng = Rng(19);
        for (w, h) in SIZES {
            let mut src = Yuv444::new(w, h);
            src.y = rng.bytes(w * h);
            src.u = rng.bytes(w * h);
            src.v = rng.bytes(w * h);
            assert!(
                max_diff(&yuv444_to_bgra(&src), &color::yuv444_to_bgra(&src)) <= 1,
                "at {w}x{h}"
            );
        }
    }
}
