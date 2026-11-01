//! Gapless presentation edits for AAC wrappers' unreported priming.

use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const MAX_MOOV_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Atom {
    start: usize,
    end: usize,
    kind: [u8; 4],
}

fn atoms(bytes: &[u8], start: usize, end: usize) -> Result<Vec<Atom>, String> {
    let mut cursor = start;
    let mut result = Vec::new();
    while cursor < end {
        let header = bytes.get(cursor..cursor + 8).ok_or("truncated MP4 atom")?;
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

fn child(bytes: &[u8], parent: Atom, kind: &[u8; 4]) -> Result<Atom, String> {
    let mut matches = atoms(bytes, parent.start + 8, parent.end)?
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

fn number(bytes: &[u8], atom: Atom, offset: usize, width: usize) -> Result<u64, String> {
    let start = atom.start + 8 + offset;
    let end = start.checked_add(width).ok_or("MP4 field overflow")?;
    if end > atom.end {
        return Err("truncated MP4 timing field".to_owned());
    }
    let field = bytes.get(start..end).ok_or("truncated MP4 timing field")?;
    Ok(field
        .iter()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte)))
}

fn version(bytes: &[u8], atom: Atom) -> Result<usize, String> {
    match number(bytes, atom, 0, 1)? {
        0 => Ok(0),
        1 => Ok(1),
        _ => Err("unsupported MP4 timing version".to_owned()),
    }
}

fn duration_field(bytes: &[u8], atom: Atom, track: bool) -> Result<(usize, usize), String> {
    let version = version(bytes, atom)?;
    Ok((
        if track {
            20 + 8 * version
        } else {
            16 + 8 * version
        },
        4 + 4 * version,
    ))
}

fn write_number(
    bytes: &mut [u8],
    atom: Atom,
    offset: usize,
    width: usize,
    value: u64,
) -> Result<(), String> {
    number(bytes, atom, offset, width)?;
    if width == 4 && value > u64::from(u32::MAX) {
        return Err("MP4 presentation duration exceeds its timing field".to_owned());
    }
    let start = atom.start + 8 + offset;
    bytes[start..start + width].copy_from_slice(&value.to_be_bytes()[8 - width..]);
    Ok(())
}

fn edit_moov(bytes: &mut [u8], frames: u64, delay: u64, rate: u32) -> Result<(), String> {
    let moov = atoms(bytes, 0, bytes.len())?
        .into_iter()
        .next()
        .ok_or("missing moov")?;
    let mvhd = child(bytes, moov, b"mvhd")?;
    let movie_version = version(bytes, mvhd)?;
    let scale = number(bytes, mvhd, 12 + movie_version * 8, 4)?;
    if scale == 0 || rate == 0 || frames == 0 {
        return Err("invalid MP4 audio presentation clock".to_owned());
    }
    let audible = u64::try_from(
        (u128::from(frames) * u128::from(scale) + u128::from(rate / 2)) / u128::from(rate),
    )
    .map_err(|_| "MP4 audio duration overflow")?;
    let tracks: Vec<_> = atoms(bytes, moov.start + 8, moov.end)?
        .into_iter()
        .filter(|atom| atom.kind == *b"trak")
        .collect();
    let mut audio_found = false;
    let mut movie_duration = 0;
    for track in tracks {
        let tkhd = child(bytes, track, b"tkhd")?;
        let (duration_offset, duration_width) = duration_field(bytes, tkhd, true)?;
        let mdia = child(bytes, track, b"mdia")?;
        let hdlr = child(bytes, mdia, b"hdlr")?;
        if number(bytes, hdlr, 8, 4)? == u64::from(u32::from_be_bytes(*b"soun")) {
            if audio_found {
                return Err("multiple MP4 audio tracks are unsupported".to_owned());
            }
            audio_found = true;
            let mdhd = child(bytes, mdia, b"mdhd")?;
            let media_version = version(bytes, mdhd)?;
            if number(bytes, mdhd, 12 + media_version * 8, 4)? != u64::from(rate) {
                return Err("MP4 audio sample clock does not match encoder input".to_owned());
            }
            let (media_offset, media_width) = duration_field(bytes, mdhd, false)?;
            let encoded_duration = number(bytes, mdhd, media_offset, media_width)?;
            let edts = child(bytes, track, b"edts")?;
            let elst = child(bytes, edts, b"elst")?;
            let edit_version = version(bytes, elst)?;
            let width = 4 + edit_version * 4;
            let entries = number(bytes, elst, 4, 4)?;
            let mut leading = 0u64;
            let mut media_found = false;
            for index in 0..entries {
                let offset = 8 + usize::try_from(index).map_err(|_| "MP4 edit index overflow")?
                    * (2 * width + 4);
                let length = number(bytes, elst, offset, width)?;
                let media = number(bytes, elst, offset + width, width)?;
                let empty = if width == 4 {
                    u64::from(u32::MAX)
                } else {
                    u64::MAX
                };
                if media == empty && !media_found {
                    leading = leading
                        .checked_add(length)
                        .ok_or("MP4 leading gap overflow")?;
                    continue;
                }
                if media_found || number(bytes, elst, offset + 2 * width, 4)? != 65_536 {
                    return Err("unsupported MP4 audio edit sequence".to_owned());
                }
                media_found = true;
                let media_start = media.checked_add(delay).ok_or("MP4 priming overflow")?;
                let signed_max = if width == 4 {
                    i32::MAX as u64
                } else {
                    i64::MAX as u64
                };
                if media_start > signed_max
                    || media_start
                        .checked_add(frames)
                        .is_none_or(|end| end > encoded_duration)
                {
                    return Err(format!(
                        "AAC presentation lies outside encoded media: start {media_start}, frames {frames}, encoded duration {encoded_duration}"
                    ));
                }
                write_number(bytes, elst, offset, width, audible)?;
                write_number(bytes, elst, offset + width, width, media_start)?;
            }
            if !media_found {
                return Err("MP4 audio has no presentation edit".to_owned());
            }
            write_number(
                bytes,
                tkhd,
                duration_offset,
                duration_width,
                leading
                    .checked_add(audible)
                    .ok_or("MP4 track duration overflow")?,
            )?;
        }
        movie_duration = movie_duration.max(number(bytes, tkhd, duration_offset, duration_width)?);
    }
    if !audio_found {
        return Err("MP4 has no AAC audio track".to_owned());
    }
    let (offset, width) = duration_field(bytes, mvhd, false)?;
    write_number(bytes, mvhd, offset, width, movie_duration)
}

pub(super) fn finalize(path: &Path, frames: u64, delay: u64, rate: u32) -> Result<(), String> {
    let operation = || -> Result<(), String> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        let length = file.metadata().map_err(|e| e.to_string())?.len();
        let mut offset = 0u64;
        while length - offset >= 8 {
            file.seek(SeekFrom::Start(offset))
                .map_err(|e| e.to_string())?;
            let mut header = [0u8; 8];
            file.read_exact(&mut header).map_err(|e| e.to_string())?;
            let short_size =
                u32::from_be_bytes(header[..4].try_into().map_err(|_| "invalid atom")?);
            let mut size = u64::from(short_size);
            let header_size = if short_size == 1 { 16 } else { 8 };
            if short_size == 1 {
                let mut extended = [0u8; 8];
                file.read_exact(&mut extended).map_err(|e| e.to_string())?;
                size = u64::from_be_bytes(extended);
            } else if short_size == 0 {
                size = length - offset;
            }
            if size < header_size || size > length - offset {
                return Err("invalid MP4 file atom".to_owned());
            }
            if &header[4..] == b"moov" {
                if size > MAX_MOOV_BYTES {
                    return Err("MP4 timing metadata is too large".to_owned());
                }
                file.seek(SeekFrom::Start(offset))
                    .map_err(|e| e.to_string())?;
                let mut moov = vec![0u8; size as usize];
                file.read_exact(&mut moov).map_err(|e| e.to_string())?;
                // Validate/edit before writing. mdat and chunk offsets stay intact.
                edit_moov(&mut moov, frames, delay, rate)?;
                file.seek(SeekFrom::Start(offset))
                    .map_err(|e| e.to_string())?;
                file.write_all(&moov).map_err(|e| e.to_string())?;
                return file.flush().map_err(|e| e.to_string());
            }
            offset += size;
        }
        Err("MP4 has no movie timing metadata".to_owned())
    };
    operation().map_err(|error| {
        format!(
            "could not finalize gapless AAC in {}: {error}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::from(((payload.len() + 8) as u32).to_be_bytes());
        bytes.extend(kind);
        bytes.extend(payload);
        bytes
    }

    fn clock(kind: &[u8; 4], version: u8, scale: u32, duration: u64) -> Vec<u8> {
        let mut payload = vec![0; if version == 0 { 32 } else { 44 }];
        payload[0] = version;
        let bytes = atom(kind, &payload);
        let a = atoms(&bytes, 0, bytes.len()).expect("test atom")[0];
        let mut bytes = bytes;
        write_number(
            &mut bytes,
            a,
            12 + usize::from(version) * 8,
            4,
            u64::from(scale),
        )
        .expect("clock scale");
        let (offset, width) =
            duration_field(&bytes, a, kind == b"tkhd").expect("clock duration field");
        write_number(&mut bytes, a, offset, width, duration).expect("clock duration");
        bytes
    }

    fn track(version: u8, audio: bool, leading: u64) -> Vec<u8> {
        let tkhd = clock(b"tkhd", version, 0, if audio { 60_000 } else { 96_000 });
        let mdhd = clock(b"mdhd", version, 48_000, 60_000);
        let mut handler = vec![0; 12];
        handler[8..12].copy_from_slice(if audio { b"soun" } else { b"vide" });
        let mdia = atom(
            b"mdia",
            &[
                mdhd,
                atom(b"hdlr", &handler),
                atom(b"stco", b"unchanged chunk offsets"),
            ]
            .concat(),
        );
        let width = if version == 0 { 4 } else { 8 };
        let mut edit = vec![version, 0, 0, 0];
        edit.extend((if leading == 0 { 1u32 } else { 2u32 }).to_be_bytes());
        for (length, media) in if leading == 0 {
            vec![(60_000, 512)]
        } else {
            vec![(leading, u64::MAX), (60_000, 512)]
        } {
            edit.extend(&length.to_be_bytes()[8 - width..]);
            edit.extend(&media.to_be_bytes()[8 - width..]);
            edit.extend(65_536u32.to_be_bytes());
        }
        atom(
            b"trak",
            &[tkhd, mdia, atom(b"edts", &atom(b"elst", &edit))].concat(),
        )
    }

    fn movie(version: u8, leading: u64) -> Vec<u8> {
        atom(
            b"moov",
            &[
                clock(b"mvhd", version, 48_000, 96_000),
                track(version, true, leading),
                track(version, false, 0),
            ]
            .concat(),
        )
    }

    #[test]
    fn presentation_edits_preserve_video_gaps_and_encoded_media_for_both_versions() {
        for version in [0, 1] {
            for leading in [0, 2_400] {
                let mut bytes = movie(version, leading);
                let moov = atoms(&bytes, 0, bytes.len()).expect("movie")[0];
                let tracks: Vec<_> = atoms(&bytes, 8, bytes.len())
                    .expect("tracks")
                    .into_iter()
                    .filter(|a| a.kind == *b"trak")
                    .collect();
                let audio = tracks[0];
                let video = bytes[tracks[1].start..tracks[1].end].to_vec();
                let mdia = child(&bytes, audio, b"mdia").expect("media");
                let media = bytes[mdia.start..mdia.end].to_vec();
                edit_moov(&mut bytes, 48_000, 2_048, 48_000).expect("gapless edit");
                assert_eq!(
                    &bytes[tracks[1].start..tracks[1].end],
                    video,
                    "video track must not move or change"
                );
                assert_eq!(
                    &bytes[mdia.start..mdia.end],
                    media,
                    "encoded duration and chunk offsets must remain intact"
                );
                let tkhd = child(&bytes, audio, b"tkhd").expect("track timing");
                let (offset, width) = duration_field(&bytes, tkhd, true).expect("duration");
                assert_eq!(
                    number(&bytes, tkhd, offset, width).expect("audio duration"),
                    leading + 48_000
                );
                let elst = child(
                    &bytes,
                    child(&bytes, audio, b"edts").expect("edits"),
                    b"elst",
                )
                .expect("edit list");
                let edit_width = 4 + usize::from(version) * 4;
                let offset = 8 + if leading == 0 { 0 } else { 2 * edit_width + 4 };
                assert_eq!(
                    number(&bytes, elst, offset, edit_width).expect("audible duration"),
                    48_000
                );
                assert_eq!(
                    number(&bytes, elst, offset + edit_width, edit_width).expect("media start"),
                    2_560
                );
                if leading != 0 {
                    assert_eq!(
                        number(&bytes, elst, 8, edit_width).expect("leading gap"),
                        leading
                    );
                }
                let mvhd = child(&bytes, moov, b"mvhd").expect("movie timing");
                let (offset, width) = duration_field(&bytes, mvhd, false).expect("duration");
                assert_eq!(
                    number(&bytes, mvhd, offset, width).expect("movie duration"),
                    96_000,
                    "video remains the longer track"
                );
            }
        }
    }

    #[test]
    fn audio_only_movie_duration_uses_the_audible_clock_and_exact_media_endpoint() {
        for version in [0, 1] {
            let mut bytes = atom(
                b"moov",
                &[
                    clock(b"mvhd", version, 1000, 60_000),
                    track(version, true, 10),
                ]
                .concat(),
            );
            edit_moov(&mut bytes, 48_000, 2_048, 48_000).expect("one second audible edit");
            let moov = atoms(&bytes, 0, bytes.len()).expect("movie")[0];
            let mvhd = child(&bytes, moov, b"mvhd").expect("movie clock");
            let (offset, width) = duration_field(&bytes, mvhd, false).expect("movie duration");
            assert_eq!(
                number(&bytes, mvhd, offset, width).expect("duration"),
                1010,
                "one second plus the existing 10ms leading gap"
            );
            let mut boundary = movie(version, 0);
            edit_moov(&mut boundary, 57_440, 2_048, 48_000)
                .expect("exact encoded endpoint must remain valid");
            let mut past = movie(version, 0);
            assert!(edit_moov(&mut past, 57_441, 2_048, 48_000)
                .expect_err("one frame beyond media must fail")
                .contains("outside encoded media"));
        }
    }

    #[test]
    fn finalization_retains_every_compressed_byte_and_file_offset() {
        let directory = tempfile::tempdir().expect("media directory");
        let path = directory.path().join("gapless.mp4");
        let data = b"every compressed priming and padding packet is required";
        let extended = [
            1u32.to_be_bytes().as_slice(),
            b"mdat",
            ((data.len() + 16) as u64).to_be_bytes().as_slice(),
            data,
        ]
        .concat();
        for payload in [atom(b"mdat", data), extended] {
            let before = [atom(b"ftyp", b"mp42"), payload, movie(0, 0)].concat();
            std::fs::write(&path, &before).expect("fixture");
            finalize(&path, 48_000, 2_048, 48_000).expect("finalization");
            let after = std::fs::read(&path).expect("finalized media");
            assert_eq!(before.len(), after.len());
            let moov_start = before.windows(4).position(|b| b == b"moov").expect("moov") - 4;
            assert_eq!(
                &before[..moov_start],
                &after[..moov_start],
                "headers/mdat and chunk offsets may not move"
            );
            assert_ne!(
                &before[moov_start..],
                &after[moov_start..],
                "presentation metadata must actually change"
            );
        }
    }

    #[test]
    fn malformed_or_unsupported_timing_preserves_the_entire_file() {
        let directory = tempfile::tempdir().expect("media directory");
        let path = directory.path().join("invalid.mp4");
        let valid = movie(0, 0);
        let moov = atoms(&valid, 0, valid.len()).expect("movie")[0];
        let mvhd = child(&valid, moov, b"mvhd").expect("clock");
        let mut zero_clock = valid.clone();
        write_number(&mut zero_clock, mvhd, 12, 4, 0).expect("invalid clock");
        let mut bad_version = valid.clone();
        bad_version[mvhd.start + 8] = 2;
        let duplicate_audio = atom(
            b"moov",
            &[
                clock(b"mvhd", 0, 48_000, 60_000),
                track(0, true, 0),
                track(0, true, 0),
            ]
            .concat(),
        );
        let missing_audio = atom(
            b"moov",
            &[clock(b"mvhd", 0, 48_000, 60_000), track(0, false, 0)].concat(),
        );
        for (bytes, frames, rate) in [
            (valid.clone(), 0, 48_000),
            (valid.clone(), 48_000, 0),
            (valid.clone(), 48_000, 48_001),
            (valid.clone(), 60_000, 48_000),
            (valid, u64::MAX, 48_000),
            (zero_clock, 48_000, 48_000),
            (bad_version, 48_000, 48_000),
            (duplicate_audio, 48_000, 48_000),
            (missing_audio, 48_000, 48_000),
            (atom(b"mdat", b"no moov"), 48_000, 48_000),
            (vec![0; 8], 48_000, 48_000),
        ] {
            std::fs::write(&path, &bytes).expect("invalid fixture");
            let error = finalize(&path, frames, 2_048, rate).expect_err("invalid timing must fail");
            assert!(
                error.contains("could not finalize gapless AAC") && error.contains("invalid.mp4"),
                "{error}"
            );
            assert_eq!(
                std::fs::read(&path).expect("original media"),
                bytes,
                "validation must precede every file write"
            );
        }
    }
}
