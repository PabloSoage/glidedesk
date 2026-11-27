//! Quality by region (H-002b): a QP offset per encoder block from the damage rectangles.
//!
//! A desktop mostly changes in small places: a typed character, a menu, a tooltip, a
//! blinking caret. Those are text and UI, where blur shows most, and they cost few bits. When
//! the damage of a frame is small, the blocks it touches get a lower QP than the rate control
//! chose; everything else keeps it. Large updates (scrolling, video, a window moving) get no
//! map: the rate control spreads the bits as before. NVENC applies the map as
//! `NV_ENC_QP_MAP_DELTA` (one value per macroblock for H.264, per 32×32 CTB for HEVC).

use crate::frame::Rect;

/// QP offset for the blocks a small update touched (6 is about twice the bits).
pub const BOOST: i8 = -6;
/// Frames whose damage covers more than this share of the picture get no map.
const MAX_SHARE: f64 = 0.125;
/// Single rectangles larger than this share are not text or UI (a window, a video).
const MAX_RECT_SHARE: f64 = 0.0625;

/// Blocks across and down a `width`×`height` picture.
pub fn blocks(width: u32, height: u32, block: u32) -> (usize, usize) {
    (
        width.div_ceil(block) as usize,
        height.div_ceil(block) as usize,
    )
}

/// Fills `map` (raster order, [`blocks`] of `width`×`height`) for one picture: [`BOOST`]
/// on the blocks of small damage rectangles, 0 elsewhere. No damage (unknown: the whole
/// frame) or a large one gives all zeros.
pub fn qp_map(width: u32, height: u32, block: u32, damage: Option<&[Rect]>, map: &mut Vec<i8>) {
    let (bw, bh) = blocks(width, height, block);
    map.clear();
    map.resize(bw * bh, 0);
    let Some(rects) = damage else { return };
    let area = f64::from(width) * f64::from(height);
    let damaged: f64 = rects.iter().map(|r| f64::from(r.w) * f64::from(r.h)).sum();
    if area == 0.0 || damaged > area * MAX_SHARE {
        return;
    }
    for r in rects {
        if f64::from(r.w) * f64::from(r.h) > area * MAX_RECT_SHARE || r.w == 0 || r.h == 0 {
            continue;
        }
        let x1 = r.x.saturating_add(r.w).min(width);
        let y1 = r.y.saturating_add(r.h).min(height);
        if r.x >= x1 || r.y >= y1 {
            continue;
        }
        let (bx0, bx1) = ((r.x / block) as usize, x1.div_ceil(block) as usize);
        let (by0, by1) = ((r.y / block) as usize, y1.div_ceil(block) as usize);
        for by in by0..by1 {
            map[by * bw + bx0..by * bw + bx1].fill(BOOST);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn unknown_damage_gives_no_map() {
        let mut map = vec![1; 3];
        qp_map(1920, 1088, 16, None, &mut map);
        assert_eq!(map.len(), 120 * 68);
        assert!(map.iter().all(|&q| q == 0));
    }

    #[test]
    fn a_small_update_raises_its_blocks() {
        let mut map = Vec::new();
        // A character typed at (100, 40), 10×18 pixels: macroblocks x 6..=6, y 2..=3.
        qp_map(1920, 1088, 16, Some(&[rect(100, 40, 10, 18)]), &mut map);
        let boosted: Vec<usize> = (0..map.len()).filter(|&i| map[i] == BOOST).collect();
        assert_eq!(boosted, vec![2 * 120 + 6, 3 * 120 + 6]);
    }

    #[test]
    fn ctbs_for_hevc() {
        let mut map = Vec::new();
        qp_map(1920, 1088, 32, Some(&[rect(0, 0, 33, 1)]), &mut map);
        assert_eq!(map.len(), 60 * 34);
        assert_eq!(&map[..3], &[BOOST, BOOST, 0]);
    }

    #[test]
    fn large_updates_keep_the_rate_control() {
        let mut map = Vec::new();
        // Scrolling: most of the screen.
        qp_map(1920, 1080, 16, Some(&[rect(0, 100, 1920, 900)]), &mut map);
        assert!(map.iter().all(|&q| q == 0));
        // Many small rectangles adding up to a large share.
        let many: Vec<Rect> = (0..40).map(|i| rect(0, i * 27, 1920, 26)).collect();
        qp_map(1920, 1080, 16, Some(&many), &mut map);
        assert!(map.iter().all(|&q| q == 0));
    }

    #[test]
    fn one_large_rectangle_is_skipped_but_not_the_small_ones() {
        let mut map = Vec::new();
        // A 480×270 video (1/16 of the screen plus a little) and a caret.
        let damage = [rect(0, 0, 481, 270), rect(1000, 500, 2, 16)];
        qp_map(1920, 1080, 16, Some(&damage), &mut map);
        assert_eq!(map.iter().filter(|&&q| q == BOOST).count(), 2);
        assert_eq!(map[31 * 120 + 62], BOOST);
        assert_eq!(map[0], 0);
    }

    #[test]
    fn rectangles_past_the_edge_are_clipped() {
        let mut map = Vec::new();
        qp_map(64, 64, 16, Some(&[rect(60, 60, 50, 50), rect(70, 0, 5, 5)]), &mut map);
        assert_eq!(map.len(), 16);
        assert_eq!(map.iter().filter(|&&q| q == BOOST).count(), 1);
        assert_eq!(map[15], BOOST);
    }
}
