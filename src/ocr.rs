//! Text recognition for scanned pages, using the OCR engine built into Windows. It runs
//! offline and adds nothing to the download. Languages are the ones installed in Windows.

use windows::{
    Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap},
    Media::Ocr::OcrEngine,
    Storage::Streams::DataWriter,
};

/// A recognised word and its box in image pixels: x0, y0, x1, y1.
pub struct Word {
    pub text: String,
    pub rect: [f32; 4],
}

#[derive(Clone)]
pub struct Engine {
    engine: OcrEngine,
    /// The largest image side the engine accepts, in pixels.
    pub max_dim: u32,
}

impl Engine {
    /// The engine for the user's Windows languages, if any of them supports OCR.
    pub fn new() -> Option<Self> {
        let engine = OcrEngine::TryCreateFromUserProfileLanguages().ok()?;
        let max_dim = OcrEngine::MaxImageDimension().unwrap_or(2600);
        Some(Self { engine, max_dim })
    }

    /// Recognises the text in a BGRA image. Returns lines of words.
    pub fn recognise(&self, bgra: &[u8], width: i32, height: i32) -> Option<Vec<Vec<Word>>> {
        let writer = DataWriter::new().ok()?;
        writer.WriteBytes(bgra).ok()?;
        let buffer = writer.DetachBuffer().ok()?;
        let bitmap = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
            &buffer,
            BitmapPixelFormat::Bgra8,
            width,
            height,
            BitmapAlphaMode::Premultiplied,
        )
        .ok()?;
        let result = self.engine.RecognizeAsync(&bitmap).ok()?.join().ok()?;
        let mut lines = Vec::new();
        for line in result.Lines().ok()? {
            let mut words = Vec::new();
            for word in line.Words().ok()? {
                let (Ok(text), Ok(r)) = (word.Text(), word.BoundingRect()) else { continue };
                words.push(Word { text: text.to_string(), rect: [r.X, r.Y, r.X + r.Width, r.Y + r.Height] });
            }
            if !words.is_empty() {
                lines.push(words);
            }
        }
        Some(lines)
    }
}
