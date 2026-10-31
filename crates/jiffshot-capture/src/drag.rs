//! Dragging files out to other applications (Explorer, chat apps, mail,
//! browsers), with the shell's own data object and drag image.

use std::io;
use std::path::PathBuf;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::Ole::{IDropSource, OleInitialize, DROPEFFECT, DROPEFFECT_COPY};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{ILCreateFromPathW, ILFree, IShellItemArray, SHCreateShellItemArrayFromIDLists, SHDoDragDrop, BHID_DataObject};

/// The shell's data object for `files` (absolute paths, any folders): it
/// offers `CF_HDROP`, the shell ID lists and file contents.
pub fn data_object(files: &[PathBuf]) -> io::Result<IDataObject> {
    let mut pidls: Vec<*const ITEMIDLIST> = Vec::with_capacity(files.len());
    let result = (|| {
        for file in files {
            let path = crate::wide(&file.to_string_lossy());
            // SAFETY: a NUL-terminated path that outlives the call.
            let pidl = unsafe { ILCreateFromPathW(PCWSTR(path.as_ptr())) };
            if pidl.is_null() {
                return Err(io::Error::new(io::ErrorKind::NotFound, format!("{} is not there", file.display())));
            }
            pidls.push(pidl);
        }
        // Absolute ID lists, so the files may come from different folders.
        // SAFETY: valid ID lists, freed below.
        let items: IShellItemArray = unsafe { SHCreateShellItemArrayFromIDLists(&pidls) }.map_err(io::Error::other)?;
        // SAFETY: plain COM call on a live array.
        unsafe { items.BindToHandler(None, &BHID_DataObject) }.map_err(io::Error::other)
    })();
    for pidl in pidls {
        // SAFETY: each one came from ILCreateFromPathW.
        unsafe { ILFree(Some(pidl)) };
    }
    result
}

/// Drags `files` from `owner` until the user drops or cancels; returns
/// whether they were dropped somewhere. Only copying is offered, so a drop
/// in Explorer never moves a capture out of the library. Call it on the UI
/// thread while the left button is down.
pub fn drag_files(owner: isize, files: &[PathBuf]) -> io::Result<bool> {
    if files.is_empty() {
        return Ok(false);
    }
    // SAFETY: OLE drag and drop needs OLE on this (STA) thread; a second
    // initialization only adds a reference.
    let _ = unsafe { OleInitialize(None) };
    let data = data_object(files)?;
    // SAFETY: the data object lives across the modal drag loop; with no drop
    // source the shell supplies its default one and the drag image.
    let effect = unsafe { SHDoDragDrop(Some(HWND(owner as *mut _)), &data, None::<&IDropSource>, DROPEFFECT_COPY) };
    match effect {
        Ok(effect) => Ok(effect != DROPEFFECT(0)),
        // DRAGDROP_S_CANCEL is a success code; anything else failed.
        Err(error) => Err(io::Error::other(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
    use windows::Win32::System::Ole::{OleUninitialize, ReleaseStgMedium};
    use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};

    #[test]
    fn data_object_offers_the_files_as_hdrop() {
        let dir = std::env::temp_dir().join(format!("jiffshot-drag-test-{}", std::process::id()));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let files = vec![dir.join("one.png"), sub.join("two.mp4")];
        for f in &files {
            std::fs::write(f, b"x").unwrap();
        }
        // SAFETY: test-thread OLE setup, undone below.
        unsafe { OleInitialize(None).unwrap() };
        let data = data_object(&files).unwrap();
        let format = FORMATETC { cfFormat: 15, ptd: std::ptr::null_mut(), dwAspect: DVASPECT_CONTENT.0, lindex: -1, tymed: TYMED_HGLOBAL.0 as u32 };
        // SAFETY: the medium is released after reading its HDROP.
        let got: Vec<PathBuf> = unsafe {
            let mut medium = data.GetData(&format).unwrap();
            let drop = HDROP(medium.u.hGlobal.0);
            let count = DragQueryFileW(drop, u32::MAX, None);
            let got = (0..count)
                .map(|i| {
                    let mut buf = vec![0u16; DragQueryFileW(drop, i, None) as usize + 1];
                    DragQueryFileW(drop, i, Some(&mut buf));
                    PathBuf::from(crate::from_wide(&buf))
                })
                .collect();
            ReleaseStgMedium(&mut medium);
            got
        };
        drop(data);
        // SAFETY: matches the OleInitialize above.
        unsafe { OleUninitialize() };
        let _ = std::fs::remove_dir_all(&dir);
        let lower = |v: &[PathBuf]| v.iter().map(|p| p.to_string_lossy().to_lowercase()).collect::<Vec<_>>();
        assert_eq!(lower(&got), lower(&files));
    }

    #[test]
    fn missing_files_fail() {
        assert!(data_object(&[PathBuf::from(r"C:\definitely\not\there\x.png")]).is_err());
    }
}
