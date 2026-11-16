//! Saving attachments to the Downloads folder.

use std::{
    fs::{File, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use gpui_kit::component::{WindowExt as _, notification::Notification};
use gpui_kit::{App, AppContext as _, Window};
use mattermost::{Client, FileInfo};

/// Download `file` into Downloads, then hand its path to `done`. The name
/// gets a numeric suffix rather than overwriting a file. A failure is
/// reported as a notification. On macOS the file is quarantined before writing.
pub fn save(
    client: Client,
    file: FileInfo,
    window: &mut Window,
    cx: &mut App,
    done: impl FnOnce(&Path, &mut Window, &mut App) + 'static,
) {
    window
        .spawn(cx, async move |cx| {
            let result = {
                let file = file.clone();
                cx.background_spawn(async move {
                    let dir = dirs::download_dir()
                        .or_else(dirs::home_dir)
                        .ok_or_else(|| anyhow::anyhow!("no Downloads folder"))?;
                    download_to(&client, &file, &dir).await
                })
                .await
            };
            let _ = cx.update(|window, cx| match result {
                Ok(path) => done(&path, window, cx),
                Err(err) => window.push_notification(
                    Notification::error(format!("Couldn’t download {}. {err:#}", file.name)),
                    cx,
                ),
            });
        })
        .detach();
}

/// Open documents and media; reveal other files for the person to inspect.
pub fn open(path: &Path, _: &mut Window, cx: &mut App) {
    #[cfg(windows)]
    let open = opens_marked_file(path, mark_internet);
    #[cfg(not(windows))]
    let open = opens_with_system(path);
    if open {
        cx.open_with_system(path);
    } else {
        cx.reveal_path(path);
    }
}

/// Do not auto-open an Internet download if its origin could not be recorded.
#[cfg(any(windows, test))]
fn opens_marked_file(path: &Path, mark: impl FnOnce(&Path) -> io::Result<()>) -> bool {
    opens_with_system(path)
        && match mark(path) {
            Ok(()) => true,
            Err(err) => {
                log::warn!(
                    "Couldn’t mark the attachment as an Internet download ({err}); showing it in the file manager"
                );
                false
            }
        }
}

#[cfg(windows)]
fn mark_internet(path: &Path) -> io::Result<()> {
    let mut zone = path.as_os_str().to_owned();
    zone.push(":Zone.Identifier");
    std::fs::write(Path::new(&zone), b"[ZoneTransfer]\r\nZoneId=3\r\n")
}

fn opens_with_system(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default();
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "pdf"
            | "txt"
            | "md"
            | "csv"
            | "json"
            | "log"
            | "rtf"
            | "doc"
            | "docx"
            | "xls"
            | "xlsx"
            | "ppt"
            | "pptx"
            | "odt"
            | "ods"
            | "odp"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "bmp"
            | "tiff"
            | "heic"
            | "mp3"
            | "m4a"
            | "wav"
            | "ogg"
            | "flac"
            | "mp4"
            | "mov"
            | "webm"
            | "mkv"
    )
}

/// Stream `file` into a new file in `dir` as it arrives, rather than holding
/// it in memory, and return where it went. A failed download leaves nothing
/// behind.
async fn download_to(client: &Client, file: &FileInfo, dir: &Path) -> anyhow::Result<PathBuf> {
    let (path, out) = claim_download(dir, &file.name)?;
    let written = client
        .download_file_to(&file.id, download_limit(file.size), move |chunk| {
            (&out).write_all(chunk)
        })
        .await;
    if let Err(err) = written {
        if let Err(remove) = std::fs::remove_file(&path) {
            log::warn!("removing the incomplete {}: {remove}", path.display());
        }
        return Err(err);
    }
    Ok(path)
}

/// The most a download may grow to, so a server can't fill the disk with an
/// endless body: the size the server gave for the file, with room to spare.
/// A file without a size (none, zero or negative; a missing size reads as
/// zero) gets a fixed bound well past the server upload limits in practice.
fn download_limit(size: i64) -> u64 {
    const SLACK: u64 = 16 * 1024 * 1024;
    const UNKNOWN: u64 = 2 * 1024 * 1024 * 1024;
    match u64::try_from(size) {
        Ok(size) if size > 0 => size.saturating_add(size / 4).saturating_add(SLACK),
        _ => UNKNOWN,
    }
}

/// Claim a new file in `dir` for `name`, quarantined on macOS before any
/// content arrives.
fn claim_download(dir: &Path, name: &str) -> io::Result<(PathBuf, File)> {
    let (path, file) = create_download(dir, name)?;
    #[cfg(target_os = "macos")]
    if let Err(err) = quarantine(&file) {
        let _ = std::fs::remove_file(&path);
        return Err(err);
    }
    Ok((path, file))
}

#[cfg(test)]
fn write_download(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    let (path, mut file) = claim_download(dir, name)?;
    file.write_all(bytes)?;
    Ok(path)
}

/// Claim a new file, retrying numbered names when another download got there first.
fn create_download(dir: &Path, name: &str) -> io::Result<(PathBuf, File)> {
    let name = Path::new(name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".into());
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), format!(".{ext}")),
        _ => (name.clone(), String::new()),
    };
    for n in 1.. {
        let path = dir.join(if n == 1 {
            name.clone()
        } else {
            format!("{stem} ({n}){ext}")
        });
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    unreachable!("an unused name exists")
}

/// Mark the claimed file before it can be opened by another application.
#[cfg(target_os = "macos")]
fn quarantine(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let value = format!("0083;{timestamp:x};mtc;");
    // SAFETY: the descriptor is live, and both buffers remain valid for the call.
    let result = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            c"com.apple.quarantine".as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Downloads(PathBuf);

    impl Downloads {
        fn new() -> Self {
            let mut bytes = [0; 16];
            getrandom::fill(&mut bytes).unwrap();
            let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../target/test-downloads")
                .join(id);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Downloads {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn downloads_stream_to_disk_and_leave_nothing_when_they_fail() {
        use mattermost::testing::{Network, Reply, SlowServer};
        use std::time::Duration;

        let server = SlowServer::start(Network::new(Duration::ZERO), |request| {
            match (request.method(), request.path()) {
                ("GET", "/api/v4/files/report") => Reply::bytes(b"quarterly".to_vec()),
                ("GET", "/api/v4/files/endless") => {
                    Reply::bytes(vec![0; 17 * 1024 * 1024]).stall_after(0)
                }
                ("GET", "/api/v4/files/unsized") => Reply::bytes(vec![7; 20 * 1024 * 1024]),
                _ => Reply::not_found(),
            }
        });
        let client = Client::new_unverified(&server.url(), "token").unwrap();
        let dir = Downloads::new();
        let file = |id: &str, size: i64| {
            let mut file = FileInfo::default();
            file.id = id.into();
            file.name = "report.pdf".into();
            file.size = size;
            file
        };

        let path =
            futures::executor::block_on(download_to(&client, &file("report", 9), &dir.0)).unwrap();
        assert_eq!(path, dir.0.join("report.pdf"));
        assert_eq!(std::fs::read(&path).unwrap(), b"quarterly");

        // Far more than the server said the file holds; and a missing file.
        for (id, size) in [("endless", 9), ("missing", 0)] {
            assert!(
                futures::executor::block_on(download_to(&client, &file(id, size), &dir.0)).is_err(),
                "{id}"
            );
        }
        let mut names: Vec<_> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names, ["report.pdf"]);

        // A file without a known size isn't held to a size-derived bound.
        let path =
            futures::executor::block_on(download_to(&client, &file("unsized", 0), &dir.0)).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 20 * 1024 * 1024);
        names.push(path.file_name().unwrap().to_owned());
        assert_eq!(names.len(), std::fs::read_dir(&dir.0).unwrap().count());
    }

    #[test]
    fn download_limits_follow_the_declared_size() {
        const SLACK: u64 = 16 * 1024 * 1024;
        const UNKNOWN: u64 = 2 * 1024 * 1024 * 1024;
        assert_eq!(download_limit(0), UNKNOWN);
        assert_eq!(download_limit(-5), UNKNOWN);
        assert_eq!(download_limit(1), 1 + SLACK);
        assert_eq!(download_limit(400), 500 + SLACK);
        assert_eq!(
            download_limit(i64::MAX),
            i64::MAX as u64 + (i64::MAX as u64) / 4 + SLACK
        );
    }

    #[test]
    fn reveals_programs_scripts_shortcuts_and_unknown_files() {
        for name in [
            "run.exe",
            "run.EXE",
            "run.cmd",
            "run.bat",
            "run.ps1",
            "run.lnk",
            "run.app",
            "run.command",
            "run.sh",
            "run.jar",
            "run.desktop",
            "run.url",
            "run.js",
            "run.vbs",
            "run.msi",
            "run.scr",
            "run.workflow",
            "run",
            "run.unknown",
            "report.pdf.exe",
            "report.pdf.exe ",
            "report.pdf.exe.",
        ] {
            assert!(!opens_with_system(Path::new(name)), "opened {name}");
        }
        for name in [
            "report.pdf",
            "report.PDF",
            "notes.txt",
            "sheet.xlsx",
            "movie.mp4",
        ] {
            assert!(opens_with_system(Path::new(name)), "didn't open {name}");
        }
    }

    #[test]
    fn opens_documents_only_after_origin_marking_succeeds() {
        let path = Path::new("report.doc");
        assert!(opens_marked_file(path, |_| Ok(())));
        assert!(!opens_marked_file(path, |_| Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "couldn't write the origin"
        ))));
        assert!(!opens_marked_file(Path::new("run.exe"), |_| panic!(
            "must reveal executable files"
        )));
    }

    #[cfg(windows)]
    #[test]
    fn records_the_internet_zone_without_changing_the_document() {
        let dir = Downloads::new();
        let path = write_download(&dir.0, "report.doc", b"document").unwrap();
        mark_internet(&path).unwrap();
        let mut zone = path.as_os_str().to_owned();
        zone.push(":Zone.Identifier");
        assert_eq!(
            std::fs::read(Path::new(&zone)).unwrap(),
            b"[ZoneTransfer]\r\nZoneId=3\r\n"
        );
        assert_eq!(std::fs::read(path).unwrap(), b"document");
    }

    #[test]
    fn concurrent_downloads_keep_their_own_contents() {
        let dir = Downloads::new();
        let barrier = std::sync::Barrier::new(8);
        let results = std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..8)
                .map(|n| {
                    let dir = &dir.0;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        let bytes = vec![n; 32];
                        let path = write_download(dir, "report.pdf", &bytes).unwrap();
                        (path, bytes)
                    })
                })
                .collect();
            tasks
                .into_iter()
                .map(|task| task.join().unwrap())
                .collect::<Vec<_>>()
        });
        for (path, bytes) in &results {
            assert_eq!(std::fs::read(path).unwrap(), *bytes);
        }
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 8);
    }

    #[test]
    fn preserves_existing_files_and_sanitizes_names() {
        let dir = Downloads::new();
        std::fs::write(dir.0.join("report.pdf"), b"original").unwrap();
        let path = write_download(&dir.0, "../../report.pdf", b"new").unwrap();
        assert_eq!(path, dir.0.join("report (2).pdf"));
        assert_eq!(
            std::fs::read(dir.0.join("report.pdf")).unwrap(),
            b"original"
        );
        assert!(create_download(&dir.0.join("missing"), "report").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn skips_dangling_symlinks() {
        let dir = Downloads::new();
        let target = dir.0.join("missing");
        std::os::unix::fs::symlink(&target, dir.0.join("report.pdf")).unwrap();
        let path = write_download(&dir.0, "report.pdf", b"new").unwrap();
        assert_eq!(path, dir.0.join("report (2).pdf"));
        assert!(!target.exists());
        assert!(dir.0.join("report.pdf").is_symlink());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn downloaded_files_are_quarantined() {
        use std::os::fd::AsRawFd as _;
        let dir = Downloads::new();
        let path = write_download(&dir.0, "run.command", b"echo hello").unwrap();
        let file = File::open(path).unwrap();
        let mut value = [0u8; 256];
        // SAFETY: the descriptor is live and the output buffer has the stated capacity.
        let len = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                c"com.apple.quarantine".as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        assert!(len > 0, "{}", io::Error::last_os_error());
        assert!(
            std::str::from_utf8(&value[..len as usize])
                .unwrap()
                .starts_with("0083;")
        );
    }
}
