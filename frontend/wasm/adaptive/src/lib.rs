//! The page's decoder for a Mac's Adaptive picture, bound for the painter
//! (`frontend/src/adaptivePicture.ts`): see [`decoder`].

pub mod decoder;

use wasm_bindgen::prelude::*;

/// One session's Adaptive picture.
#[wasm_bindgen]
pub struct Adaptive {
    decoder: decoder::Decoder,
}

#[wasm_bindgen]
impl Adaptive {
    #[wasm_bindgen(constructor)]
    pub fn new() -> Adaptive {
        Adaptive { decoder: decoder::Decoder::new() }
    }

    /// A black framebuffer of the desktop's new size.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.decoder.resize(width as usize, height as usize);
    }

    /// Decode one rectangle onto the framebuffer, and say whether it drew. Throws
    /// for a rectangle that does not decode, whose pixels the framebuffer keeps
    /// as far as they were drawn.
    pub fn decode(&mut self, x: u16, y: u16, w: u16, h: u16, body: &[u8]) -> Result<bool, JsError> {
        self.decoder.decode(x, y, w, h, body).map_err(|error| JsError::new(&error.to_string()))
    }

    /// Where the framebuffer is in the module's memory: its width times its
    /// height pixels, each R, G, B, A, which stay there until the next resize.
    pub fn pixels(&self) -> *const u32 {
        self.decoder.pixels().as_ptr()
    }
}

impl Default for Adaptive {
    fn default() -> Self {
        Self::new()
    }
}
