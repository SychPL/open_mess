//! A track's WAV file: 16 kHz, mono, 16-bit PCM, with the plain 44-byte header open_mess writes
//! (spec 2026-10-28-local-transcription, 4.2). The reader takes only that layout and checks every
//! size against the file, since a file on disk may be cut short by a crash or changed by anyone.

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub const SAMPLE_RATE: u32 = 16_000;
const HEADER: u64 = 44;
/// Six hours of one track: the longest recording (4.2), and the most the reader takes.
pub const MAX_DATA: u64 = 6 * 60 * 60 * SAMPLE_RATE as u64 * 2;

fn header(data: u32) -> [u8; HEADER as usize] {
    let mut bytes = [0u8; HEADER as usize];
    let fields: [&[u8]; 13] = [
        b"RIFF",
        &data.saturating_add(36).to_le_bytes(),
        b"WAVE",
        b"fmt ",
        &16u32.to_le_bytes(),
        // PCM, one channel.
        &1u16.to_le_bytes(),
        &1u16.to_le_bytes(),
        &SAMPLE_RATE.to_le_bytes(),
        &(SAMPLE_RATE * 2).to_le_bytes(),
        // Block align, bits per sample.
        &2u16.to_le_bytes(),
        &16u16.to_le_bytes(),
        b"data",
        &data.to_le_bytes(),
    ];
    let mut at = 0;
    for field in fields {
        if let Some(slot) = bytes.get_mut(at..at + field.len()) {
            slot.copy_from_slice(field);
        }
        at += field.len();
    }
    bytes
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("not a recording track: {what}"))
}

/// Appends samples to a new track; [`Writer::finish`] writes the final sizes into the header.
pub struct Writer {
    file: BufWriter<File>,
    data: u64,
}

impl Writer {
    /// Creates the file; an existing one is never overwritten.
    pub fn create(path: &Path) -> io::Result<Writer> {
        let mut file = BufWriter::new(OpenOptions::new().write(true).create_new(true).open(path)?);
        file.write_all(&header(0))?;
        Ok(Writer { file, data: 0 })
    }

    /// Fails once the track would pass [`MAX_DATA`].
    pub fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        let bytes = samples.len() as u64 * 2;
        if self.data + bytes > MAX_DATA {
            return Err(io::Error::other("the track is longer than six hours"));
        }
        for sample in samples {
            self.file.write_all(&sample.to_le_bytes())?;
        }
        self.data += bytes;
        Ok(())
    }

    pub fn finish(self) -> io::Result<()> {
        let mut file = self.file.into_inner().map_err(io::IntoInnerError::into_error)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header(u32::try_from(self.data).map_err(io::Error::other)?))?;
        file.sync_all()
    }
}

/// Writes the sizes of a track whose writer never finished (the daemon stopped while recording)
/// from the file's length; an odd last byte is cut off.
pub fn fix_header(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    if len < HEADER {
        return Err(invalid("shorter than its header"));
    }
    let data = ((len - HEADER) & !1).min(MAX_DATA);
    file.set_len(HEADER + data)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(&header(u32::try_from(data).map_err(io::Error::other)?))?;
    file.sync_all()
}

/// Reads a track's samples in pieces.
pub struct Reader {
    file: BufReader<File>,
    left: u64,
}

impl Reader {
    pub fn open(path: &Path) -> io::Result<Reader> {
        let mut file = File::open(path)?;
        let len = file.metadata()?.len();
        let mut found = [0u8; HEADER as usize];
        file.read_exact(&mut found).map_err(|_| invalid("shorter than its header"))?;
        let data = found.get(40..44).and_then(|bytes| bytes.try_into().ok()).map(u32::from_le_bytes).ok_or_else(|| invalid("no size"))?;
        if found != header(data) {
            return Err(invalid("not 16 kHz mono 16-bit PCM"));
        }
        let data = u64::from(data);
        if data % 2 != 0 || data > MAX_DATA || HEADER + data > len {
            return Err(invalid("its size does not match the file"));
        }
        Ok(Reader { file: BufReader::new(file), left: data })
    }

    /// Samples still to read.
    pub fn remaining(&self) -> u64 {
        self.left / 2
    }

    /// Up to `max` more samples, appended to `out`; 0 at the end.
    pub fn read(&mut self, out: &mut Vec<i16>, max: usize) -> io::Result<usize> {
        let count = self.remaining().min(max as u64) as usize;
        let mut bytes = vec![0u8; count * 2];
        self.file.read_exact(&mut bytes)?;
        out.extend(bytes.as_chunks::<2>().0.iter().map(|pair| i16::from_le_bytes(*pair)));
        self.left -= count as u64 * 2;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(path: &Path) -> io::Result<Vec<i16>> {
        let mut reader = Reader::open(path)?;
        let mut out = Vec::new();
        while reader.read(&mut out, 3)? > 0 {}
        Ok(out)
    }

    #[test]
    fn written_samples_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("me.wav");
        let mut writer = Writer::create(&path).unwrap();
        writer.write(&[1, -2, i16::MAX, i16::MIN]).unwrap();
        writer.write(&[7]).unwrap();
        writer.finish().unwrap();
        assert_eq!(read_all(&path).unwrap(), vec![1, -2, i16::MAX, i16::MIN, 7]);
        // Never over an existing file.
        assert_eq!(Writer::create(&path).err().unwrap().kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn an_unfinished_track_is_fixed_from_its_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("others.wav");
        let mut writer = Writer::create(&path).unwrap();
        writer.write(&[5, 6, 7]).unwrap();
        // The daemon stopped: the header still says 0 and half a sample is on disk.
        drop(writer);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[9]).unwrap();
        drop(file);
        assert_eq!(read_all(&path).unwrap(), Vec::<i16>::new());
        fix_header(&path).unwrap();
        assert_eq!(read_all(&path).unwrap(), vec![5, 6, 7]);
    }

    #[test]
    fn bad_files_are_errors_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.wav");
        let cases: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"RIFF".to_vec(),
            // A declared size past the end of the file.
            [header(1000).as_slice(), &[0, 0]].concat(),
            // An odd size.
            [header(3).as_slice(), &[0, 0, 0]].concat(),
            // 44.1 kHz.
            header(0).iter().enumerate().map(|(i, b)| if i == 24 { 0x44 } else { *b }).collect(),
            // A size over six hours.
            header(u32::MAX).to_vec(),
        ];
        for bytes in cases {
            std::fs::write(&path, &bytes).unwrap();
            assert_eq!(read_all(&path).unwrap_err().kind(), io::ErrorKind::InvalidData, "{bytes:?}");
        }
        std::fs::write(&path, b"RIF").unwrap();
        assert!(fix_header(&path).is_err());
    }
}
