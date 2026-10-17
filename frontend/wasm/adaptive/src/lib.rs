//! The page's decoder for a Mac's own picture, bound for the painter
//! (`frontend/src/adaptivePicture.ts`): its Adaptive quality's 8×8 tiles
//! ([`decoder`]) or its Full quality's zlib ([`zlib`]).

pub mod decoder;
pub mod zlib;

use wasm_bindgen::prelude::*;

/// One session's picture, in the one quality it was started with.
#[wasm_bindgen]
pub struct Adaptive {
    picture: Picture,
}

enum Picture {
    Tiles(Box<decoder::Decoder>),
    Zlib(zlib::Decoder),
}

#[wasm_bindgen]
impl Adaptive {
    /// A picture of the Mac's Full quality where `full`, otherwise of its
    /// Adaptive quality.
    #[wasm_bindgen(constructor)]
    pub fn new(full: bool) -> Adaptive {
        let picture = if full { Picture::Zlib(zlib::Decoder::new()) } else { Picture::Tiles(Box::default()) };
        Adaptive { picture }
    }

    /// A black framebuffer of the desktop's new size.
    pub fn resize(&mut self, width: u32, height: u32) {
        match &mut self.picture {
            Picture::Tiles(decoder) => decoder.resize(width as usize, height as usize),
            Picture::Zlib(decoder) => decoder.resize(width as usize, height as usize),
        }
    }

    /// Decode one rectangle onto the framebuffer, and say whether it drew. Throws
    /// for a rectangle that does not decode, whose pixels the framebuffer keeps
    /// as far as they were drawn.
    pub fn decode(&mut self, x: u16, y: u16, w: u16, h: u16, body: &[u8]) -> Result<bool, JsError> {
        match &mut self.picture {
            Picture::Tiles(decoder) => decoder.decode(x, y, w, h, body).map_err(|error| JsError::new(&error.to_string())),
            Picture::Zlib(decoder) => decoder.decode(x, y, w, h, body).map_err(|error| JsError::new(&error.to_string())),
        }
    }

    /// Where the framebuffer is in the module's memory: its width times its
    /// height pixels, each R, G, B, A, which stay there until the next resize.
    pub fn pixels(&self) -> *const u32 {
        match &self.picture {
            Picture::Tiles(decoder) => decoder.pixels().as_ptr(),
            Picture::Zlib(decoder) => decoder.pixels().as_ptr(),
        }
    }
}
