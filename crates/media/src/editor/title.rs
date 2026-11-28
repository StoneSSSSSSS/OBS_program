use anyhow::{ensure, Result};
use cv_core::editor::Canvas;
use image::RgbaImage;
use std::{mem::size_of, ptr::null_mut};
use windows::{
    core::w,
    Win32::{
        Foundation::{COLORREF, HANDLE, RECT},
        Graphics::Gdi::*,
    },
};

struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    font: HFONT,
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.dc);
            let _ = DeleteObject(self.font);
            let _ = DeleteObject(self.bitmap);
        }
    }
}

pub(super) fn draw(canvas: Canvas, text: &str) -> Result<RgbaImage> {
    draw_sized(canvas, text, ((canvas.width.min(canvas.height) / 10) as i32).max(12))
}

pub(super) fn draw_sized(canvas: Canvas, text: &str, initial_font: i32) -> Result<RgbaImage> {
    let width = canvas.width as i32;
    let height = canvas.height as i32;
    let mut text: Vec<u16> = text.encode_utf16().collect();
    let mut surface = Surface {
        dc: unsafe { CreateCompatibleDC(HDC::default()) },
        bitmap: HBITMAP::default(),
        font: HFONT::default(),
    };
    ensure!(!surface.dc.is_invalid(), "Cannot create the title card surface");
    let mut pixels = null_mut();
    let bitmap = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    surface.bitmap = unsafe { CreateDIBSection(surface.dc, &bitmap, DIB_RGB_COLORS, &mut pixels, HANDLE::default(), 0) }?;
    let pixels = pixels.cast::<u8>();
    let len = canvas.width as usize * canvas.height as usize * 4;
    unsafe { std::ptr::write_bytes(pixels, 0, len) };
    let flags = DT_CENTER | DT_EDITCONTROL | DT_NOPREFIX | DT_WORDBREAK;
    let margin = width / 12;
    let mut font_size = initial_font;
    unsafe {
        SelectObject(surface.dc, surface.bitmap);
        SetBkMode(surface.dc, TRANSPARENT);
        SetTextColor(surface.dc, COLORREF(0x00ff_ffff));
        loop {
            surface.font = CreateFontW(
                -font_size, 0, 0, 0, FW_BOLD.0 as i32, 0, 0, 0,
                DEFAULT_CHARSET.0 as u32, OUT_DEFAULT_PRECIS.0 as u32,
                CLIP_DEFAULT_PRECIS.0 as u32, ANTIALIASED_QUALITY.0 as u32,
                DEFAULT_PITCH.0 as u32, w!("Segoe UI"),
            );
            ensure!(!surface.font.is_invalid(), "Cannot create the title card font");
            let previous = SelectObject(surface.dc, surface.font);
            let mut measure = RECT { left: margin, top: 0, right: width - margin, bottom: 0 };
            let lines = DrawTextW(surface.dc, &mut text, &mut measure, flags | DT_CALCRECT);
            if (lines <= height * 4 / 5 && measure.right <= width - margin) || font_size <= 12 {
                let top = ((height - lines) / 2).max(height / 10);
                let mut target = RECT { left: margin, top, right: width - margin, bottom: height - height / 10 };
                ensure!(DrawTextW(surface.dc, &mut text, &mut target, flags) > 0, "Cannot draw the title card");
                let _ = GdiFlush();
                break;
            }
            SelectObject(surface.dc, previous);
            let _ = DeleteObject(surface.font);
            surface.font = HFONT::default();
            font_size = (font_size * 3 / 4).max(12);
        }
    }
    let mut rgba = unsafe { std::slice::from_raw_parts(pixels, len) }.to_vec();
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    RgbaImage::from_raw(canvas.width, canvas.height, rgba).ok_or_else(|| anyhow::anyhow!("Invalid title card dimensions"))
}
