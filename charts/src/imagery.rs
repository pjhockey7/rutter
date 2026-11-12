//! Raster imagery from an MBTiles file.
//!
//! The same container as vector charts, carrying JPEG or PNG per tile
//! instead of gzipped protobuf — which is why this reuses [`MbTiles`]
//! wholesale and only differs in what it does with the bytes.
//!
//! Decoding happens here rather than in the renderer so that `rutter-render`
//! never learns what a JPEG is: it receives RGBA and uploads it. That keeps
//! the image decoders out of the crate that has to work identically on four
//! Android ABIs and a desktop.

use std::path::Path;

use rutter_core::chart::{TileId, TileImage};
use rutter_core::View;

use crate::mbtiles::{self, MbTiles, Metadata};

/// An open raster tile set, with a cache of decoded tiles.
#[derive(Debug)]
pub struct ImagerySource {
    file: MbTiles,
    /// Decoded tiles, most recently used last.
    ///
    /// Smaller than the vector cache: a decoded 256×256 RGBA tile is 256 KB,
    /// so sixty-four of them is 16 MB — already more than a phone should
    /// hold for scenery. Vector tiles are a few kilobytes each and can be
    /// cached far more freely.
    cache: Vec<TileImage>,
    capacity: usize,
}

/// Whether a tile carries no picture at all.
///
/// A tile service answers an area it has no imagery for in one of two
/// ways. A 404 is the easy case. The other — which the USGS endpoint
/// uses — is **HTTP 200 with a fully transparent image**, and that one is
/// invisible: it stores, it decodes, it draws nothing, and it counts as a
/// success. One real download stored 6,150 of them.
///
/// The test is exactly "is every pixel fully transparent", and it has to
/// be, because the obvious shortcuts are both wrong:
///
/// * **Size** does not separate them. The blanks are 872 bytes and the
///   smallest real tile measured is 2,010 — a ten-byte margin.
/// * **Format** does not either. Real tiles at the edge of coverage are
///   PNG precisely *because* they are part imagery and part transparent,
///   and a jpeg-only rule discarded 848 of them.
///
/// Only called for formats that can carry alpha; a JPEG cannot be blank
/// in this sense and is never decoded for it.
pub fn is_blank_tile(bytes: &[u8]) -> bool {
    // JPEG has no alpha channel, so it always carries something.
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return false;
    }

    let Ok(image) = image::load_from_memory(bytes) else {
        // Undecodable is not blank — it is broken, and the caller counts
        // that differently.
        return false;
    };
    let rgba = image.to_rgba8();
    rgba.pixels().all(|p| p.0[3] == 0)
}

impl ImagerySource {
    /// Decoded tiles held in memory. See the note on `cache`.
    pub const DEFAULT_CAPACITY: usize = 64;

    /// Most tiles [`Self::collect`] will decode for one view.
    ///
    /// Lower than the vector equivalent because each one is a JPEG decode
    /// and a quarter-megabyte allocation, not a protobuf parse.
    pub const MAX_TILES: usize = 128;

    /// Open a raster MBTiles file.
    ///
    /// Refuses a vector one: the formats share a container but nothing else,
    /// and silently drawing nothing would be a poor way to report the
    /// mistake.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, mbtiles::Error> {
        let file = MbTiles::open(path)?;
        if file.metadata().is_vector() {
            return Err(mbtiles::Error::NotRaster(file.metadata().format.clone()));
        }
        Ok(Self {
            file,
            cache: Vec::new(),
            capacity: Self::DEFAULT_CAPACITY,
        })
    }

    pub fn metadata(&self) -> &Metadata {
        self.file.metadata()
    }

    /// Every tile address the file holds at a zoom level.
    ///
    /// Passed through from the underlying file. Useful for reporting what
    /// a download actually got, and for tests that should ask what is in
    /// a file rather than assume — a hard-coded tile address stops being
    /// true the first time the file is rebuilt over a different area.
    pub fn tiles_at(&self, zoom: u8) -> Result<Vec<TileId>, mbtiles::Error> {
        self.file.tiles_at(zoom)
    }

    pub fn clamp_zoom(&self, zoom: u8) -> u8 {
        self.file.clamp_zoom(zoom)
    }

    pub fn cached(&self) -> usize {
        self.cache.len()
    }

    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// Fetch and decode one tile, or `None` if the file has no such tile.
    pub fn tile(&mut self, id: TileId) -> Result<Option<&TileImage>, mbtiles::Error> {
        if let Some(index) = self.cache.iter().position(|t| t.id == id) {
            let entry = self.cache.remove(index);
            self.cache.push(entry);
            return Ok(self.cache.last());
        }

        let Some(raw) = self.file.raw_tile(id)? else {
            return Ok(None);
        };

        let decoded = decode(id, &raw).map_err(mbtiles::Error::Image)?;

        if self.cache.len() >= self.capacity {
            self.cache.remove(0);
        }
        self.cache.push(decoded);
        Ok(self.cache.last())
    }

    /// Every imagery tile covering a view, decoded.
    ///
    /// Mirrors `ChartSource::collect`, including its tile budget: a view far
    /// outside the imagery's zoom range would otherwise ask for thousands of
    /// JPEG decodes.
    pub fn collect(&mut self, view: &View) -> Vec<TileImage> {
        let zoom = self.clamp_zoom(view.tile_zoom());
        let (nw, se) = view.bounds();
        let wanted = rutter_core::chart::tiles_in_view(nw, se, zoom);

        if wanted.len() > Self::MAX_TILES {
            log::debug!(
                "imagery: {} tiles wanted at zoom {zoom}, over the {} budget",
                wanted.len(),
                Self::MAX_TILES
            );
            return Vec::new();
        }

        let mut out = Vec::new();
        for id in wanted {
            match self.tile(id) {
                Ok(Some(image)) => out.push(image.clone()),
                Ok(None) => {}
                // One unreadable tile should leave a hole, not blank the
                // imagery: a damaged file is still worth most of its tiles.
                Err(e) => log::warn!("imagery {id:?}: {e}"),
            }
        }
        out
    }
}

/// Decode a tile's bytes to RGBA.
///
/// The format is sniffed from the bytes rather than taken from the file's
/// `format` metadata. Writers are not reliable about it, and guessing wrong
/// here produces a decode error rather than a wrong picture, so sniffing
/// costs nothing and tolerates a mislabelled file.
fn decode(id: TileId, bytes: &[u8]) -> Result<TileImage, image::ImageError> {
    let decoded = image::load_from_memory(bytes)?.to_rgba8();
    Ok(TileImage {
        id,
        width: decoded.width(),
        height: decoded.height(),
        rgba: decoded.into_raw(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest valid PNG: 1×1, opaque red.
    ///
    /// Written out by hand so the decoder is exercised without a fixture
    /// file; the bytes are a standard minimal PNG.
    fn red_pixel_png() -> Vec<u8> {
        use std::io::Cursor;
        let mut out = Vec::new();
        let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]));
        image
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .expect("encoding a 1x1 png should not fail");
        out
    }

    #[test]
    fn decodes_png_to_rgba() {
        let id = TileId { z: 13, x: 1, y: 2 };
        let tile = decode(id, &red_pixel_png()).expect("valid png");

        assert_eq!(tile.id, id);
        assert_eq!((tile.width, tile.height), (1, 1));
        assert_eq!(tile.rgba, vec![255, 0, 0, 255]);
        assert!(tile.is_consistent());
    }

    #[test]
    fn rejects_bytes_that_are_not_an_image() {
        assert!(decode(TileId::default(), b"not an image at all").is_err());
        assert!(decode(TileId::default(), &[]).is_err());
    }

    #[test]
    fn a_truncated_image_is_an_error_not_a_panic() {
        let full = red_pixel_png();
        // Keep the signature, lose the data.
        assert!(decode(TileId::default(), &full[..full.len() / 2]).is_err());
    }
}
