//! Local PDF rasterization for source evidence. No model, network, or storage IO.
use eliteforge_agent_core::AgentError;
#[derive(Clone, Copy)]
pub struct PdfEvidenceOptions {
    pub max_pages: usize,
    pub max_side: u32,
}
impl Default for PdfEvidenceOptions {
    fn default() -> Self {
        Self {
            max_pages: 16,
            max_side: 1600,
        }
    }
}
#[derive(Debug)]
pub struct RenderedPdfPage {
    /// One-based page in the supplied PDF, not a page in its original Office source.
    pub page: u32,
    pub width: u32,
    pub height: u32,
    /// Displayed crop-box size in PDF points, after the page rotation.
    pub width_points: f64,
    pub height_points: f64,
    pub png: Vec<u8>,
}
fn invalid() -> AgentError {
    AgentError::InvalidConfiguration(
        "local PDF evidence is invalid, unsupported or exceeds rendering limits".into(),
    )
}
/// A host may provide a safely converted PDF. The SDK only rasterizes bounded bytes.
/// CoreGraphics is used on macOS; unsupported platforms fail explicitly.
pub fn render_pdf_evidence(
    bytes: &[u8],
    options: PdfEvidenceOptions,
) -> Result<Vec<RenderedPdfPage>, AgentError> {
    if bytes.is_empty()
        || bytes.len() > 20 * 1024 * 1024
        || !bytes.starts_with(b"%PDF-")
        || !(1..=32).contains(&options.max_pages)
        || !(128..=2048).contains(&options.max_side)
    {
        return Err(invalid());
    }
    #[cfg(target_os = "macos")]
    {
        native::render(bytes, options)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(invalid())
    }
}
#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use std::{ffi::c_void, ptr};
    type Handle = *mut c_void;
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Point {
        x: f64,
        y: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Size {
        width: f64,
        height: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Rect {
        origin: Point,
        size: Size,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Transform {
        a: f64,
        b: f64,
        c: f64,
        d: f64,
        tx: f64,
        ty: f64,
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGDataProviderCreateWithData(
            info: Handle,
            data: *const c_void,
            size: usize,
            release: Option<unsafe extern "C" fn(Handle, *const c_void, usize)>,
        ) -> Handle;
        fn CGDataProviderRelease(provider: Handle);
        fn CGPDFDocumentCreateWithProvider(provider: Handle) -> Handle;
        fn CGPDFDocumentRelease(document: Handle);
        fn CGPDFDocumentGetNumberOfPages(document: Handle) -> usize;
        fn CGPDFDocumentIsEncrypted(document: Handle) -> bool;
        fn CGPDFDocumentGetPage(document: Handle, number: usize) -> Handle;
        fn CGPDFPageGetBoxRect(page: Handle, kind: i32) -> Rect;
        fn CGPDFPageGetRotationAngle(page: Handle) -> i32;
        fn CGPDFPageGetDrawingTransform(
            page: Handle,
            kind: i32,
            rect: Rect,
            rotation: i32,
            preserve: bool,
        ) -> Transform;
        fn CGColorSpaceCreateDeviceRGB() -> Handle;
        fn CGColorSpaceRelease(space: Handle);
        fn CGBitmapContextCreate(
            data: Handle,
            width: usize,
            height: usize,
            bits: usize,
            stride: usize,
            space: Handle,
            info: u32,
        ) -> Handle;
        fn CGContextRelease(context: Handle);
        fn CGContextSetRGBFillColor(context: Handle, red: f64, green: f64, blue: f64, alpha: f64);
        fn CGContextFillRect(context: Handle, rect: Rect);
        fn CGContextConcatCTM(context: Handle, transform: Transform);
        fn CGContextScaleCTM(context: Handle, sx: f64, sy: f64);
        fn CGContextDrawPDFPage(context: Handle, page: Handle);
    }
    // Each owning CF-style pointer is released exactly once on every exit path.
    struct Owned(Handle, unsafe extern "C" fn(Handle));
    impl Owned {
        fn new(handle: Handle, release: unsafe extern "C" fn(Handle)) -> Result<Self, AgentError> {
            if handle.is_null() {
                Err(invalid())
            } else {
                Ok(Self(handle, release))
            }
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe { (self.1)(self.0) }
        }
    }
    pub(super) fn render(
        bytes: &[u8],
        options: PdfEvidenceOptions,
    ) -> Result<Vec<RenderedPdfPage>, AgentError> {
        // Provider borrows bytes only inside this synchronous call; document drops before provider.
        let provider = Owned::new(
            unsafe {
                CGDataProviderCreateWithData(
                    ptr::null_mut(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    None,
                )
            },
            CGDataProviderRelease,
        )?;
        let doc = Owned::new(
            unsafe { CGPDFDocumentCreateWithProvider(provider.0) },
            CGPDFDocumentRelease,
        )?;
        let count = unsafe { CGPDFDocumentGetNumberOfPages(doc.0) };
        if count == 0 || count > options.max_pages || unsafe { CGPDFDocumentIsEncrypted(doc.0) } {
            return Err(invalid());
        }
        let space = Owned::new(
            unsafe { CGColorSpaceCreateDeviceRGB() },
            CGColorSpaceRelease,
        )?;
        let mut result = Vec::new();
        let mut total_bytes = 0usize;
        let mut total_pixels = 0u64;
        for index in 1..=count {
            let page = unsafe { CGPDFDocumentGetPage(doc.0, index) };
            if page.is_null() {
                return Err(invalid());
            }
            let box_rect = unsafe { CGPDFPageGetBoxRect(page, 1) }; // crop box
            let Size { width, height } = box_rect.size;
            let rotation = unsafe { CGPDFPageGetRotationAngle(page) }.rem_euclid(360);
            if !matches!(rotation, 0 | 90 | 180 | 270) {
                return Err(invalid());
            }
            let (width, height) = if matches!(rotation, 90 | 270) {
                (height, width)
            } else {
                (width, height)
            };
            if !width.is_finite()
                || !height.is_finite()
                || width <= 0.0
                || height <= 0.0
                || width > 100000.0
                || height > 100000.0
            {
                return Err(invalid());
            }
            let width_points = width;
            let height_points = height;
            let scale = f64::from(options.max_side) / width.max(height);
            let width = (width * scale).round().max(1.0) as u32;
            let height = (height * scale).round().max(1.0) as u32;
            total_pixels += u64::from(width) * u64::from(height);
            if total_pixels > 32 * 1024 * 1024 {
                return Err(invalid());
            }
            let mut pixels = vec![255u8; width as usize * height as usize * 4];
            // RGBA, premultiplied last, big-endian byte order. Opaque white background.
            let ctx = Owned::new(
                unsafe {
                    CGBitmapContextCreate(
                        pixels.as_mut_ptr().cast(),
                        width as usize,
                        height as usize,
                        8,
                        width as usize * 4,
                        space.0,
                        (4 << 12) | 1,
                    )
                },
                CGContextRelease,
            )?;
            let rect = Rect {
                origin: Point { x: 0.0, y: 0.0 },
                size: Size {
                    width: f64::from(width),
                    height: f64::from(height),
                },
            };
            unsafe {
                CGContextSetRGBFillColor(ctx.0, 1.0, 1.0, 1.0, 1.0);
                CGContextFillRect(ctx.0, rect);
                // CoreGraphics may center a smaller page without enlarging it.
                // Apply point-to-pixel scaling explicitly, then map the crop box
                // and intrinsic rotation in its own physical point space.
                CGContextScaleCTM(
                    ctx.0,
                    f64::from(width) / width_points,
                    f64::from(height) / height_points,
                );
                let physical_rect = Rect {
                    origin: Point { x: 0.0, y: 0.0 },
                    size: Size {
                        width: width_points,
                        height: height_points,
                    },
                };
                CGContextConcatCTM(
                    ctx.0,
                    CGPDFPageGetDrawingTransform(page, 1, physical_rect, 0, true),
                );
                CGContextDrawPDFPage(ctx.0, page);
            }
            drop(ctx); // release context before reading borrowed output bytes
            let image = image::RgbaImage::from_raw(width, height, pixels).ok_or_else(invalid)?;
            let mut png = std::io::Cursor::new(Vec::new());
            image
                .write_to(&mut png, image::ImageFormat::Png)
                .map_err(|_| invalid())?;
            let png = png.into_inner();
            total_bytes = total_bytes.saturating_add(png.len());
            if total_bytes > 32 * 1024 * 1024 {
                return Err(invalid());
            }
            result.push(RenderedPdfPage {
                page: index as u32,
                width,
                height,
                width_points,
                height_points,
                png,
            });
        }
        Ok(result)
    }
}
