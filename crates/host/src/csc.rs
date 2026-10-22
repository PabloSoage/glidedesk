//! RGB → YUV on the CPU for encoders without the CUDA path (VA-API, software).
//!
//! The same conversion as the CUDA kernel in `color.cu` (full-range BT.709 in integer
//! arithmetic; main view NV12 and optionally the AVC444v2 auxiliary view), on
//! BGRx frames in system memory, one task per row of 2×2 blocks.

use rayon::prelude::*;

/// An NV12 frame in system memory: luma plane then interleaved chroma, both `pitch` wide,
/// `height` rows of luma (a multiple of 16, see [`crate::color::coded_size`]).
pub struct Nv12Frame {
    pub data: Vec<u8>,
    pub pitch: usize,
    pub width: u32,
    pub height: u32,
}

impl Nv12Frame {
    pub fn new(width: u32, height: u32) -> Self {
        let pitch = width as usize;
        Self {
            data: vec![0; pitch * height as usize * 3 / 2],
            pitch,
            width,
            height,
        }
    }

    pub fn planes(&self) -> (&[u8], &[u8]) {
        self.data.split_at(self.pitch * self.height as usize)
    }
}

#[inline]
fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Full-range BT.709 luma and chroma of one BGRx pixel. FreeRDP's luma coefficients
/// (54, 183, 18) add up to 255, which turns white into 254 and darkens every grey a little;
/// 54, 183, 19 add up to 256 and are still BT.709 (0.2126, 0.7152, 0.0722 × 256).
#[inline(always)]
fn yuv(p: &[u8]) -> (u8, u8, u8) {
    let (b, g, r) = (i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
    let y = (54 * r + 183 * g + 19 * b) >> 8;
    let u = clamp8(((-29 * r - 99 * g + 128 * b) >> 8) + 128);
    let v = clamp8(((128 * r - 116 * g - 12 * b) >> 8) + 128);
    (y as u8, u, v)
}

/// Converts one source row into `y` (written straight into the luma plane) and the chroma
/// scratch rows, repeating the last pixel into the padding.
#[inline]
fn row(src: &[u8], src_w: usize, y: &mut [u8], u: &mut [u8], v: &mut [u8]) {
    for (((p, y), u), v) in src[..src_w * 4]
        .as_chunks::<4>()
        .0
        .iter()
        .zip(y.iter_mut())
        .zip(u.iter_mut())
        .zip(v.iter_mut())
    {
        (*y, *u, *v) = yuv(p);
    }
    let w = u.len();
    let (ly, lu, lv) = (y[src_w - 1], u[src_w - 1], v[src_w - 1]);
    y[src_w..w].fill(ly);
    u[src_w..w].fill(lu);
    v[src_w..w].fill(lv);
}

/// Chroma of the two rows of a block row, per pixel.
struct Scratch {
    u: [Vec<u8>; 2],
    v: [Vec<u8>; 2],
}

/// Converts a BGRx frame (`src_w`×`src_h`, `stride` bytes per row) into `main` and, when
/// given, the AVC444v2 auxiliary view `aux` (both the coded size; edge pixels repeated).
pub fn convert(
    src: &[u8],
    stride: usize,
    src_w: u32,
    src_h: u32,
    main: &mut Nv12Frame,
    aux: Option<&mut Nv12Frame>,
) {
    let (w, h, pitch) = (main.width as usize, main.height as usize, main.pitch);
    let (src_w, src_h) = (src_w as usize, src_h as usize);
    let (half, quarter) = (src_w / 2, src_w / 4);
    let (my, muv) = main.data.split_at_mut(pitch * h);
    let rows = my.par_chunks_mut(2 * pitch).zip(muv.par_chunks_mut(pitch));
    let scratch = || Scratch {
        u: [vec![0; w], vec![0; w]],
        v: [vec![0; w], vec![0; w]],
    };

    let block_row = |s: &mut Scratch,
                     by: usize,
                     ly: &mut [u8],
                     luv: &mut [u8],
                     aux: Option<(&mut [u8], &mut [u8])>| {
        let y = by * 2;
        let (ly0, ly1) = ly.split_at_mut(pitch);
        let [u0, u1] = &mut s.u;
        let [v0, v1] = &mut s.v;
        let sy0 = y.min(src_h - 1) * stride;
        let sy1 = (y + 1).min(src_h - 1) * stride;
        row(&src[sy0..], src_w, &mut ly0[..w], u0, v0);
        row(&src[sy1..], src_w, &mut ly1[..w], u1, v1);
        for (bx, uv) in luv[..w].as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let x = 2 * bx;
            let su =
                u32::from(u0[x]) + u32::from(u0[x + 1]) + u32::from(u1[x]) + u32::from(u1[x + 1]);
            let sv =
                u32::from(v0[x]) + u32::from(v0[x + 1]) + u32::from(v1[x]) + u32::from(v1[x + 1]);
            uv[0] = (su / 4) as u8;
            uv[1] = (sv / 4) as u8;
        }

        // Auxiliary view (MS-RDPEGFX 3.3.8.3.3), laid out on the real frame size.
        let Some((ay, auv)) = aux else { return };
        if y >= src_h {
            return;
        }
        let (ay0, ay1) = ay.split_at_mut(pitch);
        for bx in 0..src_w.div_ceil(2) {
            let x = 2 * bx;
            // B4/B5: U and V of the odd columns, left and right halves of the luma plane.
            ay0[bx] = u0[x + 1];
            ay0[half + bx] = v0[x + 1];
            ay1[bx] = u1[x + 1];
            ay1[half + bx] = v1[x + 1];
            // B6..B9: U and V of the even columns of the odd rows.
            let (k, plane) = (bx >> 1, bx & 1);
            auv[2 * k + plane] = u1[x];
            auv[2 * (quarter + k) + plane] = v1[x];
        }
    };

    match aux {
        None => rows
            .enumerate()
            .for_each_init(scratch, |s, (by, (ly, luv))| {
                block_row(s, by, ly, luv, None)
            }),
        Some(aux) => {
            let (ay, auv) = aux.data.split_at_mut(pitch * h);
            rows.zip(ay.par_chunks_mut(2 * pitch).zip(auv.par_chunks_mut(pitch)))
                .enumerate()
                .for_each_init(scratch, |s, (by, ((ly, luv), (ay, auv)))| {
                    block_row(s, by, ly, luv, Some((ay, auv)))
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: usize, h: usize, bgr: [u8; 3]) -> Vec<u8> {
        (0..w * h)
            .flat_map(|_| [bgr[0], bgr[1], bgr[2], 0])
            .collect()
    }

    #[test]
    fn white_black_and_red() {
        let mut m = Nv12Frame::new(16, 16);
        convert(&solid(16, 16, [255, 255, 255]), 64, 16, 16, &mut m, None);
        let (y, uv) = m.planes();
        assert!(y.iter().all(|&v| v == 255));
        assert!(uv.iter().all(|&v| v == 128));

        convert(&solid(16, 16, [0, 0, 0]), 64, 16, 16, &mut m, None);
        assert!(m.planes().0.iter().all(|&v| v == 0));

        // Pure red: Y = 54*255>>8 = 53, U = (-29*255>>8)+128 = 99, V = (128*255>>8)+128 = 255.
        convert(&solid(16, 16, [0, 0, 255]), 64, 16, 16, &mut m, None);
        let (y, uv) = m.planes();
        assert_eq!(y[0], 53);
        assert_eq!((uv[0], uv[1]), (99, 255));
    }

    #[test]
    fn padding_repeats_edges_and_aux_layout() {
        // 10×6 frame coded as 16×16 with one red pixel in the last column and row: the padding
        // repeats it, and the auxiliary view carries its chroma.
        let mut src = solid(10, 6, [0, 0, 0]);
        src[(5 * 10 + 9) * 4..(5 * 10 + 9) * 4 + 3].copy_from_slice(&[0, 0, 255]);
        let (mut m, mut a) = (Nv12Frame::new(16, 16), Nv12Frame::new(16, 16));
        convert(&src, 40, 10, 6, &mut m, Some(&mut a));
        let (y, _) = m.planes();
        assert_eq!(y[5 * 16 + 9], 53);
        assert_eq!(y[15 * 16 + 15], 53);
        assert_eq!(y[0], 0);
        // Odd column 9 of row 5: B4 at x = 9 / 2 = 4, B5 at half (5) + 4.
        let (ay, _) = a.planes();
        assert_eq!(ay[5 * 16 + 4], 99);
        assert_eq!(ay[5 * 16 + 9], 255);
    }
}

#[cfg(test)]
mod bench {
    use super::*;

    /// `cargo test --release -p glidedesk-host csc_speed -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn csc_speed() {
        for (w, h) in [(1280u32, 720u32), (1920, 1080), (3840, 2160)] {
            let src: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
            let (cw, ch) = crate::color::coded_size(w, h);
            let (mut m, mut a) = (Nv12Frame::new(cw, ch), Nv12Frame::new(cw, ch));
            for aux in [false, true] {
                let t = std::time::Instant::now();
                convert(&src, w as usize * 4, w, h, &mut m, aux.then_some(&mut a));
                for _ in 0..20 {
                    convert(&src, w as usize * 4, w, h, &mut m, aux.then_some(&mut a));
                }
                println!(
                    "{w}x{h} aux={aux}: {:.2} ms",
                    t.elapsed().as_secs_f64() * 1000.0 / 20.0
                );
            }
        }
    }
}
