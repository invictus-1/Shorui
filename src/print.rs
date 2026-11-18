//! Printing through Windows GDI. PDFium draws each page straight onto the printer's device
//! context, so text and vector art stay sharp at the printer's own resolution.

use std::path::PathBuf;

use windows::{
    Win32::{
        Graphics::Gdi::{CreateDCW, DeleteDC, GetDeviceCaps, HDC, HORZRES, VERTRES},
        Storage::Xps::{DOCINFOW, EndDoc, EndPage, StartDocW, StartPage},
        UI::Controls::Dialogs::{PD_NOSELECTION, PD_PAGENUMS, PD_RETURNDC, PRINTDLGW, PrintDlgW},
    },
    core::{HSTRING, PCWSTR, w},
};

/// Where a print job goes.
pub enum Job {
    /// A printer the user picked in the Windows print dialog: device context and page range.
    Dialog { dc: isize, from: u32, to: u32 },
    /// A named printer writing to a file, with no dialog. Used to test printing.
    File { printer: String, output: PathBuf, pages: u32 },
}

/// Shows the Windows print dialog. Returns `None` if the user cancels. Runs on its own thread
/// so the document keeps rendering while the dialog is open.
pub fn dialog(pages: u32) -> Option<Job> {
    let last = pages.clamp(1, 65_535) as u16;
    let mut pd = PRINTDLGW {
        lStructSize: std::mem::size_of::<PRINTDLGW>() as u32,
        Flags: PD_RETURNDC | PD_NOSELECTION,
        nFromPage: 1,
        nToPage: last,
        nMinPage: 1,
        nMaxPage: last,
        nCopies: 1,
        ..Default::default()
    };
    if !unsafe { PrintDlgW(&mut pd) }.as_bool() {
        return None;
    }
    let (from, to) = if (pd.Flags & PD_PAGENUMS).0 != 0 {
        (pd.nFromPage.max(1) as u32 - 1, pd.nToPage.max(1) as u32 - 1)
    } else {
        (0, pages.saturating_sub(1))
    };
    Some(Job::Dialog { dc: pd.hDC.0 as isize, from, to: to.max(from) })
}

/// Runs a print job. `render` draws one page onto the device context, given the printable
/// width and height in device pixels. Returns how many pages were sent.
pub fn run(job: Job, pages: u32, mut render: impl FnMut(u32, HDC, i32, i32) -> bool) -> Result<u32, String> {
    if pages == 0 {
        return Err("The document has no pages.".into());
    }
    unsafe {
        let (dc, from, to, output) = match job {
            Job::Dialog { dc, from, to } => (HDC(dc as _), from, to.min(pages - 1), None),
            Job::File { printer, output, pages: count } => {
                let dc = CreateDCW(w!("WINSPOOL"), &HSTRING::from(printer), PCWSTR::null(), None);
                (dc, 0, count.clamp(1, pages) - 1, Some(HSTRING::from(output.as_os_str())))
            }
        };
        if dc.is_invalid() {
            return Err("The printer isn't available.".into());
        }
        let info = DOCINFOW {
            cbSize: std::mem::size_of::<DOCINFOW>() as i32,
            lpszDocName: w!("Shorui"),
            lpszOutput: output.as_ref().map(|o| PCWSTR(o.as_ptr())).unwrap_or(PCWSTR::null()),
            ..Default::default()
        };
        if StartDocW(dc, &info) <= 0 {
            let _ = DeleteDC(dc);
            return Err("The printer refused the job.".into());
        }
        let (w, h) = (GetDeviceCaps(Some(dc), HORZRES), GetDeviceCaps(Some(dc), VERTRES));
        let mut count = 0;
        for page in from..=to {
            if StartPage(dc) <= 0 {
                break;
            }
            if render(page, dc, w, h) {
                count += 1;
            }
            EndPage(dc);
        }
        EndDoc(dc);
        let _ = DeleteDC(dc);
        Ok(count)
    }
}
