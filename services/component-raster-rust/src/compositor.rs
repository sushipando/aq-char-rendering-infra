//! Pillow-exact straight-alpha source-over blending and simple RGBA canvas
//! helpers, shared with the compose worker (see its `compositor.rs` for the
//! full derivation). The raster worker uses these to place the cropped
//! component into the transparent downsample halo patch exactly like Pillow's
//! `alpha_composite`.

#[inline(always)]
pub fn blend_pixel(src: [u8; 4], dst: [u8; 4]) -> [u8; 4] {
    let sa = src[3] as u32;
    if sa == 0 {
        return dst;
    }
    let (sr, sg, sb) = (src[0] as u32, src[1] as u32, src[2] as u32);
    let (dr, dg, db) = (dst[0] as u32, dst[1] as u32, dst[2] as u32);
    let da = dst[3] as u32;

    let blend = da * (255 - sa);
    let outa255 = sa * 255 + blend;
    let coef1 = (sa * 255 * 255 * 128) / outa255;
    let coef2 = (255u32 * 128).wrapping_sub(coef1);

    let shift = |tmp: u32| (((tmp >> 8) + tmp) >> 8) >> 7;
    let r = shift(
        (sr.wrapping_mul(coef1))
            .wrapping_add(dr.wrapping_mul(coef2))
            .wrapping_add(0x80 << 7),
    );
    let g = shift(
        (sg.wrapping_mul(coef1))
            .wrapping_add(dg.wrapping_mul(coef2))
            .wrapping_add(0x80 << 7),
    );
    let b = shift(
        (sb.wrapping_mul(coef1))
            .wrapping_add(db.wrapping_mul(coef2))
            .wrapping_add(0x80 << 7),
    );
    let a = (((outa255 + 0x80) >> 8) + outa255 + 0x80) >> 8;
    [r as u8, g as u8, b as u8, a as u8]
}

#[derive(Clone, Debug)]
pub struct RgbaImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

impl RgbaImage {
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Self {
        debug_assert_eq!(pixels.len(), width as usize * height as usize * 4);
        RgbaImage {
            width,
            height,
            pixels,
        }
    }

    /// Draw `layer` at integer offset (`x`, `y`), clipping like Pillow.
    pub fn composite(&mut self, layer: &RgbaImage, x: i64, y: i64) {
        let iw = layer.width as i64;
        let ih = layer.height as i64;
        let canvas_w = self.width as i64;
        let canvas_h = self.height as i64;
        let x0 = x.max(0);
        let y0 = y.max(0);
        let x1 = (x + iw).min(canvas_w);
        let y1 = (y + ih).min(canvas_h);
        if x0 >= x1 || y0 >= y1 || x0 >= canvas_w || y0 >= canvas_h {
            return;
        }
        let left = (x0 - x) as usize;
        let top = (y0 - y) as usize;
        for row in y0..y1 {
            let dst_row = row as usize;
            let src_row = (top + (row - y0) as usize) * layer.width as usize;
            let mut dst_off = (dst_row * self.width as usize + x0 as usize) * 4;
            let mut src_off = (src_row + left) * 4;
            let end = dst_off + (x1 - x0) as usize * 4;
            while dst_off < end {
                let out = blend_pixel(
                    [
                        layer.pixels[src_off],
                        layer.pixels[src_off + 1],
                        layer.pixels[src_off + 2],
                        layer.pixels[src_off + 3],
                    ],
                    [
                        self.pixels[dst_off],
                        self.pixels[dst_off + 1],
                        self.pixels[dst_off + 2],
                        self.pixels[dst_off + 3],
                    ],
                );
                self.pixels[dst_off..dst_off + 4].copy_from_slice(&out);
                dst_off += 4;
                src_off += 4;
            }
        }
    }

    /// Inclusive-exclusive alpha bounds like PIL's ``getbbox`` (right/bottom
    /// exclusive), or `None` when fully transparent.
    pub fn alpha_bbox(&self, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
        let mut left = width;
        let mut top = height;
        let mut right = 0u32;
        let mut bottom = 0u32;
        for y in 0..height {
            let row = &self.pixels[(y * width * 4) as usize..((y + 1) * width * 4) as usize];
            // Track whether this row has updated `top`, including x == 0.
            let mut x = false;
            for (index, pixel) in row.as_chunks::<4>().0.iter().enumerate() {
                if pixel[3] != 0 {
                    left = left.min(index as u32);
                    right = right.max(index as u32 + 1);
                    if !x {
                        top = top.min(y);
                        x = true;
                    }
                    bottom = bottom.max(y + 1);
                }
            }
        }
        if right == 0 || bottom == 0 {
            None
        } else {
            Some((left, top, right, bottom))
        }
    }

    pub fn crop(&self, bbox: (u32, u32, u32, u32)) -> RgbaImage {
        let (left, top, right, bottom) = bbox;
        let width = right - left;
        let height = bottom - top;
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for y in top..bottom {
            let start = (y * self.width + left) as usize * 4;
            let end = start + width as usize * 4;
            pixels.extend_from_slice(&self.pixels[start..end]);
        }
        RgbaImage::new(width, height, pixels)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn alpha_bounds_include_rows_starting_at_the_first_column() {
        let image = super::RgbaImage::new(1, 3, vec![0, 0, 0, 0, 255, 0, 0, 255, 0, 0, 0, 0]);
        let bbox = image.alpha_bbox(1, 3).unwrap();
        assert_eq!(bbox, (0, 1, 1, 2));
        let cropped = image.crop(bbox);
        assert_eq!((cropped.width, cropped.height), (1, 1));
    }
    use super::*;

    #[test]
    fn blend_matches_known_pillow_values() {
        assert_eq!(
            blend_pixel([200, 100, 50, 255], [0, 0, 0, 0]),
            [200, 100, 50, 255]
        );
        assert_eq!(
            blend_pixel([200, 100, 50, 128], [10, 20, 30, 128]),
            [137, 73, 43, 192]
        );
        let dst = [11, 22, 33, 44];
        assert_eq!(blend_pixel([200, 100, 50, 0], dst), dst);
    }

    #[test]
    fn alpha_bbox_and_crop() {
        let mut image = RgbaImage::new(5, 4, vec![0u8; 5 * 4 * 4]);
        // Fill pixel (2, 1) and (3, 2).
        image.pixels[(5 + 2) * 4 + 3] = 255;
        image.pixels[(2 * 5 + 3) * 4 + 3] = 128;
        let bbox = image.alpha_bbox(5, 4).unwrap();
        assert_eq!(bbox, (2, 1, 4, 3));
        let cropped = image.crop(bbox);
        assert_eq!(cropped.width, 2);
        assert_eq!(cropped.height, 2);
        assert_eq!(cropped.pixels[3], 255);
        assert_eq!(cropped.pixels[(2 + 1) * 4 + 3], 128);
        let empty = RgbaImage::new(3, 3, vec![0u8; 3 * 3 * 4]);
        assert_eq!(empty.alpha_bbox(3, 3), None);
    }
}
