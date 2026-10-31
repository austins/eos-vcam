//! BGR to NV12 (BT.709, limited range), matching the media type the source advertises.

use eosvcam_shared::shm::{HEIGHT, WIDTH};

const W: usize = WIDTH as usize;
const H: usize = HEIGHT as usize;

/// Converts a tightly packed 1280x720 24bpp BGR image into an NV12 frame.
pub fn bgr_to_nv12(bgr: &[u8], nv12: &mut [u8]) {
    assert_eq!(bgr.len(), W * H * 3);
    assert_eq!(nv12.len(), W * H * 3 / 2);
    let (y_plane, uv_plane) = nv12.split_at_mut(W * H);

    for (row, out) in bgr
        .as_chunks::<{ W * 3 }>()
        .0
        .iter()
        .zip(y_plane.as_chunks_mut::<W>().0)
    {
        for (px, y) in row.as_chunks::<3>().0.iter().zip(out) {
            let (b, g, r) = (i32::from(px[0]), i32::from(px[1]), i32::from(px[2]));
            *y = (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16) as u8;
        }
    }

    for (pair, out) in bgr
        .as_chunks::<{ W * 6 }>()
        .0
        .iter()
        .zip(uv_plane.as_chunks_mut::<W>().0)
    {
        let (top, bottom) = pair.split_at(W * 3);
        for (x, uv) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let (mut b, mut g, mut r) = (0, 0, 0);
            for p in [
                &top[x * 6..],
                &top[x * 6 + 3..],
                &bottom[x * 6..],
                &bottom[x * 6 + 3..],
            ] {
                b += i32::from(p[0]);
                g += i32::from(p[1]);
                r += i32::from(p[2]);
            }
            uv[0] = (((-26 * r - 86 * g + 112 * b + 512) >> 10) + 128) as u8;
            uv[1] = (((112 * r - 102 * g - 10 * b + 512) >> 10) + 128) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(b: u8, g: u8, r: u8) -> (u8, u8, u8) {
        let bgr: Vec<u8> = [b, g, r].repeat(W * H);
        let mut nv12 = vec![0; W * H * 3 / 2];
        bgr_to_nv12(&bgr, &mut nv12);
        (nv12[0], nv12[W * H], nv12[W * H + 1])
    }

    #[test]
    fn known_colours() {
        assert_eq!(flat(0, 0, 0), (16, 128, 128));
        assert_eq!(flat(255, 255, 255), (235, 128, 128));
        let (y, u, v) = flat(0, 0, 255);
        assert!(
            (i32::from(y) - 63).abs() <= 1
                && (i32::from(u) - 102).abs() <= 1
                && (i32::from(v) - 240).abs() <= 1
        );
    }
}
