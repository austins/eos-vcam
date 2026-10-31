//! JPEG live-view frame -> 1280x720 NV12, using the Windows Imaging Component.

use anyhow::Result;
use eosvcam_shared::shm::{FRAME_BYTES, HEIGHT, WIDTH};
use eosvcam_shared::windows_bindings::{
    CLSCTX_INPROC_SERVER, CLSID_WICImagingFactory, CoCreateInstance, GUID_WICPixelFormat24bppBGR,
    IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapInterpolationModeLinear,
    WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand, WICRect,
};
use windows_core::Interface;

use crate::convert::bgr_to_nv12;

pub struct Decoder {
    factory: IWICImagingFactory,
    bgr: Vec<u8>,
}

impl Decoder {
    /// COM must already be initialised on the calling thread.
    pub fn new() -> Result<Self> {
        // SAFETY: standard in-process COM activation.
        let factory =
            unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)? };
        Ok(Self {
            factory,
            bgr: vec![0; (WIDTH * HEIGHT * 3) as usize],
        })
    }

    /// Decodes, centre-crops to 16:9 and scales to the output size. Returns the JPEG's own size.
    pub fn decode(&mut self, jpeg: &[u8], nv12: &mut [u8]) -> Result<(u32, u32)> {
        assert_eq!(nv12.len(), FRAME_BYTES);
        // SAFETY: the WIC objects below only borrow `jpeg` until CopyPixels returns.
        unsafe {
            let stream = self.factory.CreateStream()?;
            stream
                .InitializeFromMemory(jpeg.as_ptr(), jpeg.len() as u32)
                .ok()?;
            let decoder = self.factory.CreateDecoderFromStream(
                &stream,
                std::ptr::null(),
                WICDecodeMetadataCacheOnDemand,
            )?;
            let frame = decoder.GetFrame(0)?;
            let (mut w, mut h) = (0, 0);
            frame.GetSize(&mut w, &mut h).ok()?;

            let (x, y, cw, ch) = crop_rect(w, h);
            let rect = WICRect {
                X: x as i32,
                Y: y as i32,
                Width: cw as i32,
                Height: ch as i32,
            };
            let clipper = self.factory.CreateBitmapClipper()?;
            clipper.Initialize(&frame, &rect).ok()?;
            let scaler = self.factory.CreateBitmapScaler()?;
            scaler
                .Initialize(&clipper, WIDTH, HEIGHT, WICBitmapInterpolationModeLinear)
                .ok()?;
            let conv = self.factory.CreateFormatConverter()?;
            conv.Initialize(
                &scaler,
                &GUID_WICPixelFormat24bppBGR,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .ok()?;
            // The generated CopyPixels wrapper mistakes the output buffer for a one-byte return
            // value (the metadata lacks its size annotation), so call the vtable directly.
            (Interface::vtable(&conv).base__.CopyPixels)(
                conv.as_raw(),
                std::ptr::null(),
                WIDTH * 3,
                self.bgr.len() as u32,
                self.bgr.as_mut_ptr(),
            )
            .ok()?;
            bgr_to_nv12(&self.bgr, nv12);
            Ok((w, h))
        }
    }
}

/// Largest centred 16:9 rectangle inside a `w` x `h` image, as (x, y, width, height).
fn crop_rect(w: u32, h: u32) -> (u32, u32, u32, u32) {
    let (cw, ch) = if u64::from(w) * 9 > u64::from(h) * 16 {
        (h * 16 / 9, h)
    } else {
        (w, w * 9 / 16)
    };
    ((w - cw) / 2, (h - ch) / 2, cw, ch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eosvcam_shared::windows_bindings::{COINIT_MULTITHREADED, CoInitializeEx};

    #[test]
    fn crop_is_centred_16_9() {
        assert_eq!(crop_rect(1920, 1080), (0, 0, 1920, 1080));
        assert_eq!(crop_rect(1056, 704), (0, 55, 1056, 594)); // 3:2 body: bars cut top and bottom
        assert_eq!(crop_rect(1000, 1000), (0, 219, 1000, 562));
        assert_eq!(crop_rect(2000, 900), (200, 0, 1600, 900)); // wider than 16:9: sides cut
    }

    /// Set EOSVCAM_TEST_JPEG to any JPEG; the test is skipped without it.
    #[test]
    fn decodes_a_jpeg_to_nv12() {
        let Ok(path) = std::env::var("EOSVCAM_TEST_JPEG") else {
            return;
        };
        let jpeg = std::fs::read(path).unwrap();
        // SAFETY: COM init for the test thread.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED as u32);
        }
        let mut nv12 = vec![0; FRAME_BYTES];
        let t0 = std::time::Instant::now();
        let (w, h) = Decoder::new().unwrap().decode(&jpeg, &mut nv12).unwrap();
        assert!(w > 0 && h > 0);
        println!("decode took {:?}", t0.elapsed());
        let luma = &nv12[..(WIDTH * HEIGHT) as usize];
        assert!(
            luma.iter().any(|&y| y > 40) && luma.iter().any(|&y| y < 200),
            "image looks blank"
        );
    }
}
