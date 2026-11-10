//! Bounded MP4 timing-atom access shared by inspection and AAC finalization.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub const MAX_MOOV_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ATOMS: usize = 65_536;

#[derive(Clone, Copy, Debug)]
pub struct Atom {
    pub start: usize,
    pub end: usize,
    pub kind: [u8; 4],
}

pub fn atoms(bytes: &[u8], start: usize, end: usize) -> Result<Vec<Atom>, String> {
    if start > end || end > bytes.len() {
        return Err("invalid MP4 atom bounds".to_owned());
    }
    let mut cursor = start;
    let mut result = Vec::new();
    while cursor < end {
        if result.len() >= MAX_ATOMS {
            return Err("too many MP4 atoms".to_owned());
        }
        let header_end = cursor.checked_add(8).ok_or("MP4 atom overflow")?;
        let header = bytes.get(cursor..header_end).ok_or("truncated MP4 atom")?;
        let size =
            u32::from_be_bytes(header[..4].try_into().map_err(|_| "invalid MP4 size")?) as usize;
        if size < 8 || size > end - cursor {
            return Err("invalid MP4 atom bounds".to_owned());
        }
        result.push(Atom {
            start: cursor,
            end: cursor + size,
            kind: header[4..8].try_into().map_err(|_| "invalid MP4 type")?,
        });
        cursor += size;
    }
    Ok(result)
}

pub fn child(bytes: &[u8], parent: Atom, kind: &[u8; 4]) -> Result<Atom, String> {
    let start = parent.start.checked_add(8).ok_or("MP4 atom overflow")?;
    let mut matches = atoms(bytes, start, parent.end)?
        .into_iter()
        .filter(|atom| &atom.kind == kind);
    let result = matches
        .next()
        .ok_or_else(|| format!("MP4 is missing {}", String::from_utf8_lossy(kind)))?;
    if matches.next().is_some() {
        return Err("duplicate MP4 timing atom".to_owned());
    }
    Ok(result)
}

pub fn number(bytes: &[u8], atom: Atom, offset: usize, width: usize) -> Result<u64, String> {
    if width > 8 {
        return Err("invalid MP4 field width".to_owned());
    }
    let start = atom
        .start
        .checked_add(8)
        .and_then(|start| start.checked_add(offset))
        .ok_or("MP4 field overflow")?;
    let end = start.checked_add(width).ok_or("MP4 field overflow")?;
    if end > atom.end {
        return Err("truncated MP4 timing field".to_owned());
    }
    let field = bytes.get(start..end).ok_or("truncated MP4 timing field")?;
    Ok(field
        .iter()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte)))
}

pub fn version(bytes: &[u8], atom: Atom) -> Result<usize, String> {
    match number(bytes, atom, 0, 1)? {
        0 => Ok(0),
        1 => Ok(1),
        _ => Err("unsupported MP4 timing version".to_owned()),
    }
}

fn movie(path: &Path) -> Result<Option<Vec<u8>>, String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    let length = file.metadata().map_err(|error| error.to_string())?.len();
    movie_in(file, length)
}

fn movie_in(mut file: impl Read + Seek, length: u64) -> Result<Option<Vec<u8>>, String> {
    let mut offset = 0u64;
    let mut count = 0;
    while length.saturating_sub(offset) >= 8 {
        if count >= MAX_ATOMS {
            return Err("too many MP4 atoms".into());
        }
        count += 1;
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| error.to_string())?;
        let mut header = [0u8; 8];
        file.read_exact(&mut header)
            .map_err(|error| error.to_string())?;
        let short_size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let (size, header_size) = match short_size {
            0 => (length - offset, 8),
            1 => {
                let mut extended = [0u8; 8];
                file.read_exact(&mut extended)
                    .map_err(|error| error.to_string())?;
                (u64::from_be_bytes(extended), 16)
            }
            size => (u64::from(size), 8),
        };
        if size < header_size || size > length - offset {
            return Err("invalid MP4 file atom".into());
        }
        if &header[4..] == b"moov" {
            if size > MAX_MOOV_BYTES || header_size != 8 {
                return Ok(None);
            }
            file.seek(SeekFrom::Start(offset))
                .map_err(|error| error.to_string())?;
            let mut bytes = vec![0; size as usize];
            file.read_exact(&mut bytes)
                .map_err(|error| error.to_string())?;
            return Ok(Some(bytes));
        }
        offset += size;
    }
    Ok(None)
}

fn decode_time(bytes: &[u8], stts: Atom, sample: u64) -> Result<Option<u128>, String> {
    let entries = number(bytes, stts, 4, 4)?;
    let mut remaining = sample.checked_sub(1).ok_or("invalid MP4 sample number")?;
    let mut time = 0u128;
    for index in 0..entries {
        let offset = 8usize
            .checked_add(
                usize::try_from(index)
                    .map_err(|_| "MP4 timing table overflow")?
                    .checked_mul(8)
                    .ok_or("MP4 timing table overflow")?,
            )
            .ok_or("MP4 timing table overflow")?;
        let count = number(bytes, stts, offset, 4)?;
        let delta = number(
            bytes,
            stts,
            offset.checked_add(4).ok_or("MP4 timing table overflow")?,
            4,
        )?;
        let consumed = remaining.min(count);
        time += u128::from(consumed) * u128::from(delta);
        remaining -= consumed;
        if remaining == 0 && consumed < count {
            return Ok(Some(time));
        }
    }
    Ok(None)
}

fn interval_from_movie(bytes: &[u8]) -> Result<Option<f64>, String> {
    let moov = atoms(bytes, 0, bytes.len())?
        .into_iter()
        .next()
        .ok_or("missing MP4 movie atom")?;
    if moov.kind != *b"moov" {
        return Err("invalid MP4 movie atom".into());
    }
    let children = atoms(bytes, moov.start + 8, moov.end)?;
    if children.iter().any(|atom| atom.kind == *b"mvex") {
        return Ok(None);
    }
    let mut video = None;
    for track in children.into_iter().filter(|atom| atom.kind == *b"trak") {
        let mdia = child(bytes, track, b"mdia")?;
        let handler = child(bytes, mdia, b"hdlr")?;
        if number(bytes, handler, 8, 4)? == u64::from(u32::from_be_bytes(*b"vide")) {
            if video.replace(mdia).is_some() {
                return Ok(None);
            }
        }
    }
    let Some(mdia) = video else {
        return Ok(None);
    };
    let mdhd = child(bytes, mdia, b"mdhd")?;
    let timescale = number(bytes, mdhd, 12 + version(bytes, mdhd)? * 8, 4)?;
    if timescale == 0 {
        return Err("invalid MP4 video timescale".into());
    }
    let minf = child(bytes, mdia, b"minf")?;
    let stbl = child(bytes, minf, b"stbl")?;
    let stts = child(bytes, stbl, b"stts")?;
    if number(bytes, stts, 0, 1)? != 0 {
        return Ok(None);
    }
    let entries = number(bytes, stts, 4, 4)?;
    let mut samples = 0u64;
    for index in 0..entries {
        let offset = 8usize
            .checked_add(
                usize::try_from(index)
                    .map_err(|_| "MP4 timing table overflow")?
                    .checked_mul(8)
                    .ok_or("MP4 timing table overflow")?,
            )
            .ok_or("MP4 timing table overflow")?;
        let count = number(bytes, stts, offset, 4)?;
        let delta = number(
            bytes,
            stts,
            offset.checked_add(4).ok_or("MP4 timing table overflow")?,
            4,
        )?;
        if count == 0 || delta == 0 {
            return Err("invalid MP4 sample timing".into());
        }
        samples = samples
            .checked_add(count)
            .ok_or("MP4 sample count overflow")?;
    }
    if samples < 2 {
        return Ok(None);
    }
    let mut sync = atoms(bytes, stbl.start + 8, stbl.end)?
        .into_iter()
        .filter(|atom| atom.kind == *b"stss");
    let (first, last, intervals) = if let Some(stss) = sync.next() {
        if sync.next().is_some() {
            return Err("duplicate MP4 sync-sample atom".into());
        }
        if number(bytes, stss, 0, 1)? != 0 {
            return Ok(None);
        }
        let count = number(bytes, stss, 4, 4)?;
        if count < 2 {
            return Ok(None);
        }
        let mut first = 0;
        let mut last = 0;
        for index in 0..count {
            let offset = 8usize
                .checked_add(
                    usize::try_from(index)
                        .map_err(|_| "MP4 sync table overflow")?
                        .checked_mul(4)
                        .ok_or("MP4 sync table overflow")?,
                )
                .ok_or("MP4 sync table overflow")?;
            let sample = number(bytes, stss, offset, 4)?;
            if sample <= last || sample > samples {
                return Err("invalid MP4 sync-sample order".into());
            }
            if index == 0 {
                first = sample;
            }
            last = sample;
        }
        (first, last, count - 1)
    } else {
        (1, samples, samples - 1)
    };
    let first_time = decode_time(bytes, stts, first)?.ok_or("missing MP4 sample timing")?;
    let last_time = decode_time(bytes, stts, last)?.ok_or("missing MP4 sample timing")?;
    Ok(Some(
        (last_time - first_time) as f64 / timescale as f64 / intervals as f64,
    ))
}

pub(crate) fn keyframe_interval(path: &Path) -> Result<Option<f64>, String> {
    match movie(path)? {
        Some(bytes) => interval_from_movie(&bytes),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(payload);
        bytes
    }

    fn video_track(sync: Option<&[u32]>) -> Vec<u8> {
        let mut mdhd = vec![0; 20];
        mdhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
        let mut handler = vec![0; 12];
        handler[8..12].copy_from_slice(b"vide");
        let mut timing = vec![0; 4];
        timing.extend_from_slice(&2u32.to_be_bytes());
        for (count, delta) in [(3u32, 100u32), (3, 200)] {
            timing.extend_from_slice(&count.to_be_bytes());
            timing.extend_from_slice(&delta.to_be_bytes());
        }
        let mut tables = atom(b"stts", &timing);
        if let Some(sync) = sync {
            let mut payload = vec![0; 4];
            payload.extend_from_slice(&(sync.len() as u32).to_be_bytes());
            for sample in sync {
                payload.extend_from_slice(&sample.to_be_bytes());
            }
            tables.extend_from_slice(&atom(b"stss", &payload));
        }
        atom(
            b"trak",
            &atom(
                b"mdia",
                &[
                    atom(b"mdhd", &mdhd),
                    atom(b"hdlr", &handler),
                    atom(b"minf", &atom(b"stbl", &tables)),
                ]
                .concat(),
            ),
        )
    }

    #[test]
    fn keyframe_spacing_uses_sync_samples_and_variable_decode_timing() {
        let movie = atom(b"moov", &video_track(Some(&[1, 3, 6])));
        let interval = interval_from_movie(&movie).unwrap().unwrap();
        assert!((interval - 0.35).abs() < 1e-12);
        let all_sync = atom(b"moov", &video_track(None));
        let interval = interval_from_movie(&all_sync).unwrap().unwrap();
        assert!((interval - 0.14).abs() < 1e-12);
        let one_sync = atom(b"moov", &video_track(Some(&[1])));
        assert_eq!(interval_from_movie(&one_sync).unwrap(), None);
    }

    #[test]
    fn keyframe_spacing_does_not_guess_for_ambiguous_or_invalid_tables() {
        let track = video_track(Some(&[1, 3, 6]));
        let multiple = atom(b"moov", &[track.clone(), track.clone()].concat());
        assert_eq!(interval_from_movie(&multiple).unwrap(), None);
        let fragmented = atom(b"moov", &[track, atom(b"mvex", &[])].concat());
        assert_eq!(interval_from_movie(&fragmented).unwrap(), None);
        for sync in [&[1, 3, 3][..], &[0, 3][..], &[1, 7][..]] {
            let invalid = atom(b"moov", &video_track(Some(sync)));
            assert!(interval_from_movie(&invalid).is_err());
        }
        let mut truncated = atom(b"moov", &video_track(Some(&[1, 3, 6])));
        let entry_count = truncated
            .windows(4)
            .position(|window| window == b"stss")
            .unwrap()
            + 8;
        truncated[entry_count..entry_count + 4].copy_from_slice(&4u32.to_be_bytes());
        assert!(interval_from_movie(&truncated).is_err());
    }

    #[test]
    fn atom_access_rejects_invalid_bounds_and_oversized_fields() {
        let bytes = atom(b"test", &[1, 2, 3, 4]);
        let test = atoms(&bytes, 0, bytes.len()).unwrap()[0];
        assert!(atoms(&bytes, 1, bytes.len() + 1).is_err());
        assert!(number(&bytes, test, 0, 9).is_err());
        assert!(number(&bytes, test, 2, 4).is_err());
        let overflow = Atom {
            start: usize::MAX,
            ..test
        };
        assert!(child(&bytes, overflow, b"test").is_err());
    }

    #[test]
    fn movie_scanning_skips_extended_media_atoms_and_bounds_work() {
        let expected = atom(b"moov", &video_track(Some(&[1, 3, 6])));
        let mut bytes = 1u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"mdat");
        bytes.extend_from_slice(&16u64.to_be_bytes());
        bytes.extend_from_slice(&expected);
        assert_eq!(
            movie_in(Cursor::new(&bytes), bytes.len() as u64).unwrap(),
            Some(expected)
        );

        let mut oversized = ((MAX_MOOV_BYTES + 1) as u32).to_be_bytes().to_vec();
        oversized.extend_from_slice(b"moov");
        assert_eq!(
            movie_in(Cursor::new(oversized), MAX_MOOV_BYTES + 1).unwrap(),
            None
        );

        let malformed = [0, 0, 0, 20, b'm', b'd', b'a', b't'];
        assert!(movie_in(Cursor::new(malformed), malformed.len() as u64).is_err());

        let many = atom(b"free", &[]).repeat(MAX_ATOMS + 1);
        assert!(movie_in(Cursor::new(&many), many.len() as u64).is_err());
    }
}
