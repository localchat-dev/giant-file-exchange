pub const ICON_SIZE: u32 = 32;

/// Returns the shared app icon as straight-alpha RGBA pixels.
pub fn icon_rgba() -> Vec<u8> {
    let size = ICON_SIZE as usize;
    let mut rgba = vec![0_u8; size * size * 4];
    for y in 0..size {
        for x in 0..size {
            let index = (y * size + x) * 4;
            let is_f = ((8..13).contains(&x) && (6..27).contains(&y))
                || ((8..25).contains(&x) && (6..11).contains(&y))
                || ((8..21).contains(&x) && (14..19).contains(&y));
            let color = if is_f {
                [255, 255, 255, 255]
            } else {
                [7, 7, 7, 255]
            };
            rgba[index..index + 4].copy_from_slice(&color);
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_opaque_black_and_white_f() {
        let rgba = icon_rgba();
        assert_eq!(rgba.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
        assert_eq!(&rgba[0..4], &[7, 7, 7, 255]);
        let f_pixel = (7 * ICON_SIZE as usize + 9) * 4;
        assert_eq!(&rgba[f_pixel..f_pixel + 4], &[255, 255, 255, 255]);
    }
}
