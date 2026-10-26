//! The narinfo `Compression:` field as a type, and the one decoder for it.
//!
//! The NAR a narinfo advertises is packed under the codec its `Compression:`
//! field names. Every reader of a cache NAR (substitution, admission to a
//! cache, an audit) has to turn that field into a decoder; doing it here once
//! means they agree on which names are legal and on what an unknown name
//! means (a typed refusal, never a guess).

use std::io::{self, Read, Write};

/// A NAR compression nix writes into the `Compression:` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarCompression {
    /// `none` (or an absent field): the bytes are the NAR.
    None,
    /// `xz`.
    Xz,
    /// `zstd`.
    Zstd,
}

impl NarCompression {
    /// Resolve a `Compression:` value; `None` for a codec sui cannot decode.
    #[must_use]
    pub fn from_narinfo_name(name: &str) -> Option<Self> {
        match name.trim() {
            "" | "none" => Some(Self::None),
            "xz" => Some(Self::Xz),
            "zstd" => Some(Self::Zstd),
            _ => None,
        }
    }

    /// The field value nix writes for this codec.
    #[must_use]
    pub fn narinfo_name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Xz => "xz",
            Self::Zstd => "zstd",
        }
    }

    /// Wrap `compressed` in a reader that yields the uncompressed NAR.
    ///
    /// # Errors
    ///
    /// When the zstd decoder cannot be initialized.
    pub fn decoder<'a, R: Read + 'a>(self, compressed: R) -> io::Result<Box<dyn Read + 'a>> {
        Ok(match self {
            Self::None => Box::new(compressed),
            Self::Xz => Box::new(xz2::read::XzDecoder::new(compressed)),
            Self::Zstd => Box::new(zstd::stream::read::Decoder::new(compressed)?),
        })
    }

    /// Decompress a whole value.
    ///
    /// # Errors
    ///
    /// When the bytes are not a valid frame for this codec.
    pub fn decompress(self, compressed: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        self.decoder(compressed)?.read_to_end(&mut out)?;
        Ok(out)
    }

    /// A [`DecodingWriter`] that decodes into `sink`.
    ///
    /// # Errors
    ///
    /// When the zstd decoder cannot be initialized.
    pub fn decoding_writer<W: Write>(self, sink: W) -> io::Result<DecodingWriter<W>> {
        Ok(match self {
            Self::None => DecodingWriter::None(sink),
            Self::Xz => DecodingWriter::Xz(xz2::write::XzDecoder::new(sink)),
            Self::Zstd => DecodingWriter::Zstd(zstd::stream::write::Decoder::new(sink)?),
        })
    }
}

/// A push-style decoder: compressed bytes are written in, the uncompressed
/// NAR comes out into the sink `W`.
///
/// The write side of [`NarCompression::decoder`], for a caller that receives
/// the NAR as a stream of chunks and must never hold it whole (a cache
/// admitting an upload, an audit reading a stored NAR).
pub enum DecodingWriter<W: Write> {
    /// `none`: bytes pass through.
    None(W),
    /// `xz`.
    Xz(xz2::write::XzDecoder<W>),
    /// `zstd`.
    Zstd(zstd::stream::write::Decoder<'static, W>),
}

impl<W: Write> DecodingWriter<W> {
    /// End the compressed stream and return the sink.
    ///
    /// # Errors
    ///
    /// When the compressed stream is truncated or corrupt.
    pub fn finish(self) -> io::Result<W> {
        match self {
            Self::None(w) => Ok(w),
            Self::Xz(mut d) => d.finish(),
            Self::Zstd(mut d) => {
                d.flush()?;
                Ok(d.into_inner())
            }
        }
    }
}

impl<W: Write> Write for DecodingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::None(w) => w.write(buf),
            Self::Xz(d) => d.write(buf),
            Self::Zstd(d) => d.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::None(w) => w.flush(),
            Self::Xz(d) => d.flush(),
            Self::Zstd(d) => d.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_codec_round_trips_and_unknown_names_are_refused() {
        let data = b"nix-archive-1 payload".repeat(100);
        let mut xz = xz2::write::XzEncoder::new(Vec::new(), 6);
        xz.write_all(&data).unwrap();
        let xz = xz.finish().unwrap();
        let zst = zstd::encode_all(&data[..], 3).unwrap();
        assert_eq!(NarCompression::Xz.decompress(&xz).unwrap(), data);
        assert_eq!(NarCompression::Zstd.decompress(&zst).unwrap(), data);
        assert_eq!(NarCompression::None.decompress(&data).unwrap(), data);
        assert!(NarCompression::Xz.decompress(&data).is_err());
        assert_eq!(NarCompression::from_narinfo_name("bzip2"), None);
        assert_eq!(NarCompression::from_narinfo_name(""), Some(NarCompression::None));
        for c in [NarCompression::None, NarCompression::Xz, NarCompression::Zstd] {
            assert_eq!(NarCompression::from_narinfo_name(c.narinfo_name()), Some(c));
        }
        for (codec, packed) in [(NarCompression::Xz, &xz), (NarCompression::Zstd, &zst), (NarCompression::None, &data)] {
            let mut w = codec.decoding_writer(Vec::new()).unwrap();
            for chunk in packed.chunks(7) {
                w.write_all(chunk).unwrap();
            }
            assert_eq!(w.finish().unwrap(), data, "{codec:?} push decode");
        }
        let mut cut = NarCompression::Xz.decoding_writer(Vec::new()).unwrap();
        cut.write_all(&xz[..xz.len() / 2]).unwrap();
        assert!(cut.finish().is_err(), "a truncated xz stream must not finish clean");
    }
}
