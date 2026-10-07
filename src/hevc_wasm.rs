//! BETA: the page's software HEVC decoder, for a browser whose own
//! `VideoDecoder` refuses a High Performance Mac's 4:4:4 picture
//! (`frontend/src/softwareDecoder.ts`).
//!
//! It is andrewtheguy/hevc-wasm's decoder, written for the Mac's stream and
//! compiled to WebAssembly, a release published to the private
//! andrewtheguy/hevc-wasm-archives, and no build of this binary holds it, since
//! its licence keeps it out of every artifact as the native decoder's keeps that
//! out ([`crate::libav`]): an operator who wants it downloads the release archive
//! into the gateway's data directory, or anywhere else and names it in
//! `[hevc_wasm]`. The
//! gateway reads that archive once at start-up, refuses it unless it is exactly the
//! release pinned here — the page's worker calls the module's exports as this
//! version has them, so any other build is one the page cannot drive — and serves
//! its two files from memory at `/hevc/` ([`crate::assets`]). Nothing else is read
//! from the archive, and nothing is read from disk after start-up.

use std::io::Read as _;
use std::path::Path;

use anyhow::{Context as _, Result, bail, ensure};
use bytes::Bytes;
use flate2::read::GzDecoder;
use sha2::{Digest as _, Sha256};

/// The hevc-wasm release this gateway's page is written against.
pub const VERSION: &str = "0.0.3";

/// The SHA-256 of that release's archive, as its `SHA256SUMS` publishes it.
const SHA256: &str = "b973fb00a981dd5a81919fe4cad86099a0ea02e83cbc37e571c2773f43a85ab9";

/// The archive's name as released, which is also `[hevc_wasm].archive`'s default.
pub fn archive_name() -> String {
    format!("hevc-wasm-v{VERSION}.tar.gz")
}

/// How the pinned archive is downloaded: through `gh`, since the repository
/// holding it is private.
pub fn download_command() -> String {
    format!(
        "gh release download v{VERSION} --repo andrewtheguy/hevc-wasm-archives --pattern {}",
        archive_name()
    )
}

/// One of the decoder's files, with the validator it is served under.
#[derive(Clone, Debug)]
pub struct DecoderFile {
    pub data: Bytes,
    /// The file's SHA-256 as hex, its `ETag`.
    pub sha256: String,
}

/// The decoder's two files, held for the life of the gateway.
#[derive(Clone, Debug)]
pub struct HevcDecoder {
    /// `hevc.js`, wasm-bindgen's ES module glue.
    js: DecoderFile,
    /// `hevc.wasm`.
    wasm: DecoderFile,
}

impl HevcDecoder {
    /// Read the release archive at `archive` and check it is the pinned one.
    pub fn load(archive: &Path) -> Result<Self> {
        let bytes = std::fs::read(archive).with_context(|| {
            format!(
                "cannot read the software HEVC decoder {} ([hevc_wasm].archive) — \
                 download it there with `{}`",
                archive.display(),
                download_command()
            )
        })?;
        Self::from_archive(&bytes, SHA256).with_context(|| {
            format!(
                "{} ([hevc_wasm].archive) is not hevc-wasm v{VERSION} — download it with `{}`",
                archive.display(),
                download_command()
            )
        })
    }

    /// The decoder in `bytes`, a gzipped tar whose SHA-256 must be `sha256`.
    fn from_archive(bytes: &[u8], sha256: &str) -> Result<Self> {
        let got = hex(&Sha256::digest(bytes));
        ensure!(got == sha256, "its SHA-256 is {got}, not the pinned {sha256}");
        let mut js = None;
        let mut wasm = None;
        let mut archive = tar::Archive::new(GzDecoder::new(bytes));
        for entry in archive.entries().context("the archive is not a gzipped tar")? {
            let mut entry = entry.context("the archive is not a gzipped tar")?;
            let path = entry.path().context("an archive entry has no path")?;
            let slot = match path.to_str() {
                Some("hevc.js") => &mut js,
                Some("hevc.wasm") => &mut wasm,
                _ => continue,
            };
            let mut data = Vec::new();
            entry
                .read_to_end(&mut data)
                .context("an archive entry does not read")?;
            let sha256 = hex(&Sha256::digest(&data));
            *slot = Some(DecoderFile { data: Bytes::from(data), sha256 });
        }
        match (js, wasm) {
            (Some(js), Some(wasm)) => Ok(Self { js, wasm }),
            _ => bail!("the archive does not hold both hevc.js and hevc.wasm"),
        }
    }

    /// The file served at `/hevc/<name>`, with its content type.
    pub fn file(&self, name: &str) -> Option<(&DecoderFile, &'static str)> {
        match name {
            "hevc.js" => Some((&self.js, "text/javascript; charset=utf-8")),
            "hevc.wasm" => Some((&self.wasm, "application/wasm")),
            _ => None,
        }
    }
}

fn hex(digest: &[u8]) -> String {
    use std::fmt::Write as _;
    digest.iter().fold(String::with_capacity(64), |mut out, byte| {
        write!(out, "{byte:02x}").expect("writing to a String cannot fail");
        out
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A gzipped tar of `files`, as the release packs its two.
    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (name, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder.append_data(&mut header, name, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    /// A decoder of two small stand-in files, for the tests that serve one.
    pub(crate) fn decoder() -> HevcDecoder {
        let bytes = archive(&[("hevc.js", b"export default 1;"), ("hevc.wasm", b"\0asm")]);
        HevcDecoder::from_archive(&bytes, &hex(&Sha256::digest(&bytes))).unwrap()
    }

    #[test]
    fn the_pinned_archive_gives_both_files() {
        let decoder = decoder();
        let (js, mime) = decoder.file("hevc.js").unwrap();
        assert_eq!(&js.data[..], b"export default 1;");
        assert_eq!(mime, "text/javascript; charset=utf-8");
        let (wasm, mime) = decoder.file("hevc.wasm").unwrap();
        assert_eq!(&wasm.data[..], b"\0asm");
        assert_eq!(wasm.sha256, hex(&Sha256::digest(b"\0asm")));
        assert_eq!(mime, "application/wasm");
        assert!(decoder.file("bench.js").is_none());
    }

    /// Any other archive, however well formed, is not the build the page drives.
    #[test]
    fn an_archive_that_is_not_the_pinned_one_is_refused() {
        let bytes = archive(&[("hevc.js", b"a"), ("hevc.wasm", b"b")]);
        let err = HevcDecoder::from_archive(&bytes, SHA256).unwrap_err();
        assert!(format!("{err:#}").contains("not the pinned"), "{err:#}");
    }

    #[test]
    fn an_archive_missing_a_file_is_refused() {
        let bytes = archive(&[("hevc.js", b"a")]);
        let err = HevcDecoder::from_archive(&bytes, &hex(&Sha256::digest(&bytes))).unwrap_err();
        assert!(format!("{err:#}").contains("both hevc.js and hevc.wasm"), "{err:#}");
    }

    #[test]
    fn a_missing_archive_says_where_to_get_it() {
        let err = HevcDecoder::load(Path::new("/nonexistent/hevc-wasm.tar.gz")).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("[hevc_wasm].archive"), "{message}");
        assert!(message.contains(&download_command()), "{message}");
    }
}
