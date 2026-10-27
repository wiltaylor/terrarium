//! PNG helpers for screenshots: read the size, decode to RGBA, crop,
//! scale, and encode. The engine crops and scales here, so every driver
//! gives the same `region` and `scale` rules. A step-2 driver (VNC, RDP)
//! that has a raw framebuffer makes an [`Image`] with
//! [`Image::from_rgba`] or [`Image::from_bgrx`] and calls
//! [`Image::encode`].

use terrarium_core::display::Region;

use super::DisplayError;

/// The PNG file signature.
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// The largest image the host agent decodes: 16384 x 16384 pixels.
const MAX_SIDE: u32 = 16_384;

/// True when the bytes start with the PNG signature.
pub fn is_png(bytes: &[u8]) -> bool {
    bytes.len() >= 24 && bytes[..8] == SIGNATURE
}

/// The width and the height from the IHDR chunk, without a decode.
pub fn dims(png: &[u8]) -> Result<(u32, u32), DisplayError> {
    if !is_png(png) || &png[12..16] != b"IHDR" {
        return Err(DisplayError::Failed(
            "the screenshot is not a PNG image".into(),
        ));
    }
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    Ok((w, h))
}

/// An 8-bit RGBA image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, rows from the top.
    pub rgba: Vec<u8>,
}

fn bad(e: impl std::fmt::Display) -> DisplayError {
    DisplayError::Failed(format!("cannot read the screenshot: {e}"))
}

impl Image {
    /// An image from RGBA bytes.
    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Result<Self, DisplayError> {
        if rgba.len() as u64 != u64::from(width) * u64::from(height) * 4 {
            return Err(bad("the pixel buffer does not match the size"));
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }

    /// An image from BGRX bytes (4 bytes for each pixel, blue first), the
    /// common layout of a VNC or RDP framebuffer.
    pub fn from_bgrx(width: u32, height: u32, bgrx: &[u8]) -> Result<Self, DisplayError> {
        if bgrx.len() as u64 != u64::from(width) * u64::from(height) * 4 {
            return Err(bad("the pixel buffer does not match the size"));
        }
        let mut rgba = Vec::with_capacity(bgrx.len());
        for p in bgrx.chunks_exact(4) {
            rgba.extend_from_slice(&[p[2], p[1], p[0], 0xff]);
        }
        Ok(Self {
            width,
            height,
            rgba,
        })
    }

    /// Decode a PNG to RGBA, whatever its color type and depth.
    pub fn decode(png_bytes: &[u8]) -> Result<Self, DisplayError> {
        let (w, h) = dims(png_bytes)?;
        if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
            return Err(bad(format!("the size {w}x{h} is out of range")));
        }
        let mut decoder = png::Decoder::new(png_bytes);
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().map_err(bad)?;
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).map_err(bad)?;
        let data = &buf[..info.buffer_size()];
        let px = (info.width as usize) * (info.height as usize);
        let rgba = match info.color_type {
            png::ColorType::Rgba => data.to_vec(),
            png::ColorType::Rgb => {
                let mut out = Vec::with_capacity(px * 4);
                for p in data.chunks_exact(3) {
                    out.extend_from_slice(&[p[0], p[1], p[2], 0xff]);
                }
                out
            }
            png::ColorType::Grayscale => {
                let mut out = Vec::with_capacity(px * 4);
                for g in data {
                    out.extend_from_slice(&[*g, *g, *g, 0xff]);
                }
                out
            }
            png::ColorType::GrayscaleAlpha => {
                let mut out = Vec::with_capacity(px * 4);
                for p in data.chunks_exact(2) {
                    out.extend_from_slice(&[p[0], p[0], p[0], p[1]]);
                }
                out
            }
            png::ColorType::Indexed => return Err(bad("an indexed PNG was not expanded")),
        };
        Self::from_rgba(info.width, info.height, rgba)
    }

    /// Encode as an RGBA PNG with fast compression.
    pub fn encode(&self) -> Result<Vec<u8>, DisplayError> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, self.width, self.height);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::Fast);
            let mut w = enc.write_header().map_err(bad)?;
            w.write_image_data(&self.rgba).map_err(bad)?;
            w.finish().map_err(bad)?;
        }
        Ok(out)
    }

    /// The part of the image in `r`. A region that starts outside the
    /// image is an error; one that runs past the edge is cut at the edge.
    pub fn crop(&self, r: Region) -> Result<Self, DisplayError> {
        if r.x >= self.width || r.y >= self.height {
            return Err(DisplayError::BadRequest(format!(
                "the region starts at {},{}, outside the {}x{} screen",
                r.x, r.y, self.width, self.height
            )));
        }
        let w = r.w.min(self.width - r.x);
        let h = r.h.min(self.height - r.y);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for row in r.y..r.y + h {
            let start = ((row * self.width + r.x) * 4) as usize;
            rgba.extend_from_slice(&self.rgba[start..start + (w * 4) as usize]);
        }
        Self::from_rgba(w, h, rgba)
    }

    /// Scale down by `factor` (0.1 to 1) with a box filter: each new pixel
    /// is the mean of the source pixels it covers.
    pub fn scale(&self, factor: f32) -> Result<Self, DisplayError> {
        if !(0.1..=1.0).contains(&factor) {
            return Err(DisplayError::BadRequest(format!(
                "scale {factor} is outside 0.1..=1"
            )));
        }
        let nw = ((self.width as f32 * factor).round() as u32).max(1);
        let nh = ((self.height as f32 * factor).round() as u32).max(1);
        if nw == self.width && nh == self.height {
            return Ok(self.clone());
        }
        let mut rgba = Vec::with_capacity((nw * nh * 4) as usize);
        for ny in 0..nh {
            let y0 = (u64::from(ny) * u64::from(self.height) / u64::from(nh)) as u32;
            let y1 = ((u64::from(ny + 1) * u64::from(self.height) / u64::from(nh)) as u32)
                .max(y0 + 1)
                .min(self.height);
            for nx in 0..nw {
                let x0 = (u64::from(nx) * u64::from(self.width) / u64::from(nw)) as u32;
                let x1 = ((u64::from(nx + 1) * u64::from(self.width) / u64::from(nw)) as u32)
                    .max(x0 + 1)
                    .min(self.width);
                let mut sum = [0u64; 4];
                for y in y0..y1 {
                    for x in x0..x1 {
                        let i = ((y * self.width + x) * 4) as usize;
                        for (c, s) in sum.iter_mut().enumerate() {
                            *s += u64::from(self.rgba[i + c]);
                        }
                    }
                }
                let n = u64::from((y1 - y0) * (x1 - x0));
                for s in sum {
                    rgba.push((s / n) as u8);
                }
            }
        }
        Self::from_rgba(nw, nh, rgba)
    }
}

/// A test image: red on the left half, blue on the right half.
#[cfg(test)]
pub(crate) fn two_tone(w: u32, h: u32) -> Image {
    let mut rgba = Vec::new();
    for _ in 0..h {
        for x in 0..w {
            if x < w / 2 {
                rgba.extend_from_slice(&[255, 0, 0, 255]);
            } else {
                rgba.extend_from_slice(&[0, 0, 255, 255]);
            }
        }
    }
    Image {
        width: w,
        height: h,
        rgba,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_round_trips() {
        let img = two_tone(8, 4);
        let png = img.encode().unwrap();
        assert!(is_png(&png));
        assert_eq!(dims(&png).unwrap(), (8, 4));
        assert_eq!(Image::decode(&png).unwrap(), img);
    }

    #[test]
    fn crop_cuts_at_the_edge_and_refuses_outside() {
        let img = two_tone(8, 4);
        let c = img
            .crop(Region {
                x: 6,
                y: 1,
                w: 10,
                h: 2,
            })
            .unwrap();
        assert_eq!((c.width, c.height), (2, 2));
        assert_eq!(&c.rgba[..4], &[0, 0, 255, 255]);
        assert!(
            img.crop(Region {
                x: 8,
                y: 0,
                w: 1,
                h: 1
            })
            .is_err()
        );
    }

    #[test]
    fn scale_averages_boxes() {
        let img = two_tone(8, 4);
        let s = img.scale(0.5).unwrap();
        assert_eq!((s.width, s.height), (4, 2));
        assert_eq!(&s.rgba[..4], &[255, 0, 0, 255]);
        assert_eq!(&s.rgba[12..16], &[0, 0, 255, 255]);
        assert!(img.scale(2.0).is_err());
        assert_eq!(img.scale(1.0).unwrap(), img);
    }

    #[test]
    fn bgrx_becomes_rgba() {
        let img = Image::from_bgrx(1, 1, &[1, 2, 3, 0]).unwrap();
        assert_eq!(img.rgba, vec![3, 2, 1, 255]);
        assert!(Image::from_bgrx(2, 1, &[0; 4]).is_err());
    }

    #[test]
    fn not_a_png_is_refused() {
        assert!(dims(b"GIF89a.................").is_err());
        assert!(Image::decode(&[0u8; 4]).is_err());
    }
}
