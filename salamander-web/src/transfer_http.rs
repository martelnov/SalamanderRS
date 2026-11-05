//! Browser <-> server transfers for the local test prototype.
//!
//! Request bodies and download responses are streamed; neither is buffered
//! completely in RAM. Directory traversal uses Linux openat2 relative to root
//! directory descriptors pinned when the server starts. This is not a substitute
//! for the container / authentication hardening needed by AI Platform.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read, Write, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::{Component, Path};

use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::Json;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

use super::{valid_entry_name, require_write_header, ApiError, AppState};

const RESOLVE_NO_SYMLINKS: u64 = 0x04;
const RESOLVE_BENEATH: u64 = 0x08;
pub(super) const RENAME_NOREPLACE: u32 = 1;

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

pub(super) fn checked_openat2(parent: RawFd, name: &CStr, flags: i32) -> io::Result<File> {
    let how = OpenHow {
        flags: flags as u64,
        mode: 0,
        resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS,
    };
    // SAFETY: openat2 is invoked with an initialized open_how and a valid
    // NUL-terminated path; a nonnegative result is an owned file descriptor.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            parent,
            name.as_ptr(),
            &how as *const OpenHow,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat2 creates a fresh fd owned by this File.
    Ok(unsafe { File::from_raw_fd(fd as RawFd) })
}

pub(super) fn open_relative_directory(root: &File, relative: &str) -> Result<File, ApiError> {
    if !Path::new(relative).components().all(|component| {
        matches!(component, Component::Normal(_) | Component::CurDir)
    }) {
        return Err(ApiError::forbidden("Cesta nesmí opustit zvolený strom."));
    }
    if relative.is_empty() || relative == "." {
        return root.try_clone().map_err(|e| io_error("Otevření kořene", e));
    }
    let name = CString::new(relative)
        .map_err(|_| ApiError::bad_request("Název obsahuje neplatný znak."))?;
    checked_openat2(root.as_raw_fd(), &name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC)
        .map_err(|e| io_error("Otevření cílového adresáře", e))
}

pub(super) fn io_error(action: &str, error: io::Error) -> ApiError {
    match error.raw_os_error() {
        Some(libc::ENOENT) => ApiError::not_found("Požadovaná položka neexistuje."),
        Some(libc::EEXIST) => ApiError::conflict("Soubor s tímto názvem již existuje; nepřepisujeme jej."),
        Some(libc::EACCES | libc::EPERM | libc::ELOOP | libc::EXDEV) =>
            ApiError::forbidden("Přístup k souboru byl odmítnut."),
        Some(libc::ENOSPC | libc::EDQUOT) =>
            ApiError::bad_request("Nedostatek místa nebo dosažená kvóta."),
        _ => ApiError::bad_request(&format!("{action}: {error}")),
    }
}

#[derive(Deserialize)]
pub(super) struct TransferQuery {
    root: String,
    #[serde(default)]
    path: String,
    name: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    new_name: Option<String>,
}

/// POST /api/file/create — create a zero-byte regular file in the selected panel.
/// The directory is resolved relative to an already-open root descriptor,
/// and O_EXCL|O_NOFOLLOW prevents overwriting existing entries or symlinks.
#[derive(Deserialize)]
pub(super) struct CreateFileRequest {
    root: String,
    #[serde(default)]
    path: String,
    name: String,
}

pub(super) async fn create_empty_file(
    State(state): State<AppState>, headers: HeaderMap,
    Json(req): Json<CreateFileRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_write_header(&headers, "create-file")?;
    if !valid_entry_name(&req.name) {
        return Err(ApiError::bad_request("Neplatný název souboru."));
    }
    let _permit = state.write_permit()?;
    tokio::task::spawn_blocking(move || {
        let root = state.root_handle(&req.root)?;
        let parent = open_relative_directory(root, &req.path)?;
        let filename = CString::new(req.name)
            .map_err(|_| ApiError::bad_request("Neplatný název souboru."))?;
        // SAFETY: parent is an open directory descriptor and filename is a
        // validated single component; O_EXCL prevents replacing any entry.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(), filename.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL |
                    libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o666, // masked by the current process umask
            )
        };
        if fd < 0 {
            return Err(io_error("Vytvoření souboru", io::Error::last_os_error()));
        }
        // SAFETY: openat returned a new, owned file descriptor.
        let _file = unsafe { File::from_raw_fd(fd) };
        Ok::<_, ApiError>(())
    }).await.map_err(|_| ApiError::bad_request("Vytvoření souboru selhalo."))??;
    Ok((StatusCode::CREATED, [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"ok": true}))))
}

/// Escape a UTF-8 filename for RFC 5987 Content-Disposition filename*=.
/// The ASCII fallback is fixed to avoid header injection and odd filenames.
fn download_disposition(name: &str) -> String {
    let mut encoded = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.') {
            encoded.push(char::from(b));
        } else {
            encoded.push_str(&format!("%{b:02X}"));
        }
    }
    format!("attachment; filename=\"download\"; filename*=UTF-8''{encoded}")
}

/// Only one explicit byte range is accepted. Invalid or multiple ranges yield 416.
fn parse_single_range(value: &str, length: u64) -> Option<(u64, u64)> {
    let range = value.strip_prefix("bytes=")?;
    if length == 0 || range.contains(',') { return None; }
    let (first, last) = range.split_once('-')?;
    if first.is_empty() {
        let suffix = last.parse::<u64>().ok()?;
        if suffix == 0 { return None; }
        return Some((length.saturating_sub(suffix), length - 1));
    }
    let start = first.parse::<u64>().ok()?;
    if start >= length { return None; }
    let end = if last.is_empty() { length - 1 }
        else { last.parse::<u64>().ok()?.min(length - 1) };
    if start > end { return None; }
    Some((start, end))
}

/// GET /api/download — one regular file, streamed directly from disk.
pub(super) async fn download_file(
    State(state): State<AppState>, Query(query): Query<TransferQuery>, headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !valid_entry_name(&query.name) {
        return Err(ApiError::bad_request("Neplatný název souboru."));
    }
    let name = query.name.clone();
    let (file, metadata) = tokio::task::spawn_blocking(move || {
        let root = state.root_handle(&query.root)?;
        let parent = open_relative_directory(root, &query.path)?;
        let filename = CString::new(query.name)
            .map_err(|_| ApiError::bad_request("Neplatný název souboru."))?;
        let file = checked_openat2(
            parent.as_raw_fd(), &filename, libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        ).map_err(|e| io_error("Otevření souboru", e))?;
        let metadata = file.metadata().map_err(|e| io_error("Čtení metadat", e))?;
        if !metadata.is_file() {
            return Err(ApiError::bad_request("Stahovat lze pouze běžné soubory."));
        }
        Ok::<_, ApiError>((file, metadata))
    }).await.map_err(|_| ApiError::bad_request("Příprava stahování selhala."))??;

    let length = metadata.len();
    // A strong file version identifier lets the browser refuse an unsafe resume
    // after a file has changed. Do not derive it from the pathname alone.
    let etag = format!("\"{:x}-{:x}-{:x}-{:x}-{:x}-{:x}-{:x}\"",
        metadata.dev(), metadata.ino(), length,
        metadata.mtime(), metadata.mtime_nsec(), metadata.ctime(), metadata.ctime_nsec());
    let requested_range = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
    let if_range_matches = headers.get(header::IF_RANGE)
        .and_then(|v| v.to_str().ok()).is_none_or(|value| value == etag);
    let range = if if_range_matches {
        match requested_range {
            Some(value) => match parse_single_range(value, length) {
                Some(value) => Some(value),
                None => {
                    return Ok(Response::builder()
                        .status(StatusCode::RANGE_NOT_SATISFIABLE)
                        .header(header::CONTENT_RANGE, format!("bytes */{length}"))
                        .header(header::ACCEPT_RANGES, "bytes")
                        .header(header::ETAG, etag)
                        .header(header::CACHE_CONTROL, "no-store")
                        .body(Body::empty()).map_err(|_| ApiError::bad_request("Chybná odpověď."))?);
                }
            },
            None => None,
        }
    } else { None };
    let (start, end) = range.unwrap_or((0, length.saturating_sub(1)));
    let count = if length == 0 { 0 } else { end - start + 1 };
    let mut file = tokio::fs::File::from_std(file);
    if start != 0 { file.seek(SeekFrom::Start(start)).await
        .map_err(|e| io_error("Nastavení pozice stahování", e))?; }
    let mut builder = Response::builder()
        .status(if range.is_some() { StatusCode::PARTIAL_CONTENT } else { StatusCode::OK })
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, count.to_string())
        .header(header::CONTENT_DISPOSITION, download_disposition(&name))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, "no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    if range.is_some() { builder = builder.header(header::CONTENT_RANGE,
        format!("bytes {start}-{end}/{length}")); }
    let response = builder.body(Body::from_stream(ReaderStream::with_capacity(
        file.take(count), 256 * 1024,
    ))).map_err(|_| ApiError::bad_request("Odpověď pro stahování se nepodařilo vytvořit."))?;
    Ok(response)
}

/// Remove a partially uploaded temporary file even on upload error or client
/// disconnect (when the request future is cancelled).
struct TempUpload {
    parent: File,
    name: CString,
    committed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UploadMode {
    Reject,
    Overwrite,
    Rename,
}

impl UploadMode {
    fn parse(value: Option<&str>) -> Result<Self, ApiError> {
        match value.unwrap_or("reject") {
            "reject" => Ok(Self::Reject),
            "overwrite" => Ok(Self::Overwrite),
            "rename" => Ok(Self::Rename),
            _ => Err(ApiError::bad_request("Neplatná volba při nahrávání.")),
        }
    }
}

pub(super) fn existing_kind(parent: &File, name: &CStr) -> io::Result<Option<libc::mode_t>> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: pinned directory descriptor, NUL-terminated filename and a valid
    // output buffer. AT_SYMLINK_NOFOLLOW prevents inspecting a link target.
    let result = unsafe {
        libc::fstatat(parent.as_raw_fd(), name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW)
    };
    if result == 0 {
        // SAFETY: the successful fstatat call filled stat.
        return Ok(Some(unsafe { stat.assume_init() }.st_mode & libc::S_IFMT));
    }
    let error = io::Error::last_os_error();
    if error.kind() == io::ErrorKind::NotFound { Ok(None) } else { Err(error) }
}

impl TempUpload {
    fn commit(mut self, final_name: &str, mode: UploadMode) -> io::Result<()> {
        let final_name = CString::new(final_name)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Neplatný název"))?;
        if mode == UploadMode::Overwrite {
            // Do not overwrite directories, symlinks or special files. The
            // rename below replaces just a directory entry, not a link target.
            if existing_kind(&self.parent, &final_name)?
                .is_some_and(|kind| kind != libc::S_IFREG)
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "Přepsat lze pouze běžný soubor, nikoli adresář či odkaz",
                ));
            }
        }
        // SAFETY: both filenames are validated single components, and both
        // dirfds point at the same pinned directory. Plain renameat atomically
        // replaces an existing regular file only in explicit Overwrite mode.
        let result = if mode == UploadMode::Overwrite {
            unsafe { libc::renameat(
                self.parent.as_raw_fd(), self.name.as_ptr(),
                self.parent.as_raw_fd(), final_name.as_ptr(),
            ) as libc::c_long }
        } else {
            // RENAME_NOREPLACE never replaces an existing entry (including a
            // dangling symlink); an upload racing another writer stays safe.
            unsafe { libc::syscall(
                libc::SYS_renameat2,
                self.parent.as_raw_fd(), self.name.as_ptr(),
                self.parent.as_raw_fd(), final_name.as_ptr(),
                RENAME_NOREPLACE,
            ) }
        };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for TempUpload {
    fn drop(&mut self) {
        if !self.committed {
            // SAFETY: name is a valid NUL-terminated single component.
            unsafe { libc::unlinkat(self.parent.as_raw_fd(), self.name.as_ptr(), 0); }
        }
    }
}

fn entry_exists(parent: &File, name: &CStr) -> io::Result<bool> {
    existing_kind(parent, name).map(|kind| kind.is_some())
}

fn make_temp_upload(parent: File, id: u64) -> io::Result<(File, TempUpload)> {
    for attempt in 0..64 {
        let name = CString::new(format!(
            ".salamander-upload-{}-{id}-{attempt}.partial", std::process::id(),
        )).expect("internally generated name contains no NUL");
        // SAFETY: parent is an open dirfd, name is one path component;
        // O_EXCL prevents replacing another file; 0600 prevents shared access.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(), name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0o600,
            )
        };
        if fd >= 0 {
            // SAFETY: openat succeeded and the fd is now owned by File.
            let file = unsafe { File::from_raw_fd(fd) };
            return Ok((file, TempUpload { parent, name, committed: false }));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "Kolize dočasných názvů"))
}

/// POST /api/upload?root=data&path=...&name=...&mode=reject|overwrite|rename
/// Raw binary request body (NOT multipart), one file per request.
/// The body is read chunk-by-chunk and written to a temporary file. Only a
/// completely received and synced file is atomically published. Overwrite is
/// possible only when explicitly requested. This remains a test-only endpoint.
pub(super) async fn upload_file(
    State(state): State<AppState>, Query(query): Query<TransferQuery>, request: Request,
) -> Result<Response, ApiError> {
    require_write_header(request.headers(), "upload")?;
    if !valid_entry_name(&query.name) {
        return Err(ApiError::bad_request("Neplatný název souboru."));
    }
    let mode = UploadMode::parse(query.mode.as_deref())?;
    let final_name = if mode == UploadMode::Rename
        || (mode == UploadMode::Overwrite && query.new_name.is_some()) {
        let new_name = query.new_name.as_deref()
            .ok_or_else(|| ApiError::bad_request("Chybí nový název souboru."))?;
        if !valid_entry_name(new_name) || (mode == UploadMode::Rename && new_name == query.name) {
            return Err(ApiError::bad_request("Neplatný nový název souboru."));
        }
        new_name.to_owned()
    } else {
        query.name.clone()
    };
    if request.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok())
        != Some("application/octet-stream") {
        return Err(ApiError {
            status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
            message: "Nahrávání vyžaduje Content-Type: application/octet-stream.".to_owned(),
        });
    }
    let max = state.max_upload_bytes;
    if request.headers().get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .is_some_and(|len| len > max) {
        return Err(too_large());
    }
    let _permit = state.write_permit()?;
    let name = final_name.clone();
    let id = state.next_job_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (raw_file, temp) = tokio::task::spawn_blocking(move || {
        let root = state.root_handle(&query.root)?;
        let parent = open_relative_directory(root, &query.path)?;
        let target = CString::new(final_name)
            .map_err(|_| ApiError::bad_request("Neplatný název souboru."))?;
        if mode != UploadMode::Overwrite
            && entry_exists(&parent, &target).map_err(|e| io_error("Kontrola cíle", e))? {
            return Err(ApiError::conflict("Soubor s tímto názvem již existuje; nepřepisujeme jej."));
        }
        if mode == UploadMode::Overwrite
            && existing_kind(&parent, &target).map_err(|e| io_error("Kontrola cíle", e))?
                .is_some_and(|kind| kind != libc::S_IFREG) {
            return Err(ApiError::forbidden("Přepsat lze pouze běžný soubor."));
        }
        make_temp_upload(parent, id).map_err(|e| io_error("Příprava nahrávání", e))
    }).await.map_err(|_| ApiError::bad_request("Příprava nahrávání selhala."))??;

    let mut file = tokio::fs::File::from_std(raw_file);
    let mut stream = request.into_body().into_data_stream();
    let mut transferred = 0u64;
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|_| ApiError::bad_request("Přenos se předčasně přerušil."))?;
        transferred = transferred.checked_add(bytes.len() as u64).ok_or_else(too_large)?;
        if transferred > max {
            return Err(too_large());
        }
        file.write_all(&bytes).await.map_err(|e| io_error("Zápis souboru", e))?;
    }
    file.flush().await.map_err(|e| io_error("Dokončení zápisu", e))?;
    file.sync_all().await.map_err(|e| io_error("Uložení souboru", e))?;
    drop(file);

    let name_for_commit = name.clone();
    tokio::task::spawn_blocking(move || temp.commit(&name_for_commit, mode))
        .await.map_err(|_| ApiError::bad_request("Dokončení nahrávání selhalo."))?
        .map_err(|e| io_error("Dokončení nahrávání", e))?;
    let mut response = (StatusCode::CREATED, Json(json!({
        "ok": true, "name": name, "bytes": transferred,
    }))).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn too_large() -> ApiError {
    ApiError {
        status: StatusCode::PAYLOAD_TOO_LARGE,
        message: "Soubor překračuje nastavený limit nahrávání.".to_owned(),
    }
}

use axum::response::IntoResponse;


// F4 — text editor. Files are opened relative to pinned root directory fds and
// symlinks are not followed. Limit memory consumption and reject binary data.
// This endpoint is NOT a replacement for a full POSIX editor: saving performs
// an atomic replacement and refuses files whose hardlinks or xattrs/ACLs could
// be silently changed. External concurrent changes are detected best-effort.
const EDIT_LIMIT: usize = 1024 * 1024;

#[derive(Deserialize)]
pub(super) struct EditorQuery {
    root: String,
    #[serde(default)]
    path: String,
    name: String,
}

#[derive(Deserialize)]
pub(super) struct EditorSaveRequest {
    root: String,
    path: String,
    name: String,
    original: String,
    text: String,
}

#[derive(Serialize)]
struct EditorDocument {
    name: String,
    text: String,
    read_only: bool,
    max_bytes: usize,
}

fn text_error(message: &str) -> ApiError {
    ApiError { status: StatusCode::UNSUPPORTED_MEDIA_TYPE, message: message.to_owned() }
}

fn validate_editor_text(bytes: Vec<u8>) -> Result<String, ApiError> {
    if bytes.len() > EDIT_LIMIT {
        return Err(ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: "Editor dovoluje soubory do 1 MiB. Větší soubor stáhni nebo uprav jiným nástrojem.".into(),
        });
    }
    if bytes.contains(&0) {
        return Err(text_error("Soubor obsahuje binární data (NUL); nelze jej otevřít v textovém editoru."));
    }
    String::from_utf8(bytes).map_err(|_| text_error("Editor podporuje pouze text ve formátu UTF-8."))
}

fn editor_parent_and_file(
    state: &AppState, root: &str, path: &str, name: &str,
) -> Result<(File, CString, File), ApiError> {
    if !valid_entry_name(name) {
        return Err(ApiError::bad_request("Neplatný název souboru."));
    }
    let parent = open_relative_directory(state.root_handle(root)?, path)?;
    let filename = CString::new(name)
        .map_err(|_| ApiError::bad_request("Neplatný název souboru."))?;
    let file = checked_openat2(
        parent.as_raw_fd(), &filename, libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
    ).map_err(|e| io_error("Otevření textového souboru", e))?;
    let metadata = file.metadata().map_err(|e| io_error("Metadata souboru", e))?;
    if !metadata.is_file() {
        return Err(ApiError::bad_request("Editor je určen pouze pro běžné textové soubory."));
    }
    if metadata.len() > EDIT_LIMIT as u64 {
        return Err(ApiError { status: StatusCode::PAYLOAD_TOO_LARGE,
            message: "Text je příliš velký pro editor (max. 1 MiB).".into() });
    }
    Ok((parent, filename, file))
}

fn bounded_read(mut file: &File) -> Result<Vec<u8>, ApiError> {
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(EDIT_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error("Čtení textu", e))?;
    if bytes.len() > EDIT_LIMIT {
        return Err(ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            message: "Text je příliš velký pro editor (max. 1 MiB).".into(),
        });
    }
    Ok(bytes)
}

/// GET /api/editor/open?root=data&path=...&name=...
pub(super) async fn open_text(
    State(state): State<AppState>, Query(query): Query<EditorQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let writable = state.writes_enabled;
    let document = tokio::task::spawn_blocking(move || {
        let (_, _, file) = editor_parent_and_file(&state, &query.root, &query.path, &query.name)?;
        let text = validate_editor_text(bounded_read(&file)?)?;
        Ok::<_, ApiError>(EditorDocument {
            name: query.name, text, read_only: !writable, max_bytes: EDIT_LIMIT,
        })
    }).await.map_err(|_| ApiError::bad_request("Otevření editoru selhalo."))??;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(document)))
}

// Replacements change the inode. Refuse files with hardlinks or xattrs/ACLs
// rather than silently losing those important server-side semantics.
fn can_replace_text_file(file: &File) -> Result<std::fs::Metadata, ApiError> {
    let metadata = file.metadata().map_err(|e| io_error("Metadata souboru", e))?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        return Err(ApiError::forbidden("Editor neuloží speciální soubory ani soubory s více pevnými odkazy."));
    }
    // SAFETY: null output buffer with size=0 queries the required list size.
    let xattrs = unsafe { libc::flistxattr(file.as_raw_fd(), std::ptr::null_mut(), 0) };
    if xattrs < 0 {
        return Err(io_error("Kontrola rozšířených atributů", io::Error::last_os_error()));
    }
    if xattrs > 0 {
        return Err(ApiError::forbidden("Soubor obsahuje ACL nebo rozšířené atributy; bezpečné zachování metadat v editoru zatím není implementované."));
    }
    Ok(metadata)
}

fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
        && left.len() == right.len() && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
}

fn save_text_blocking(state: &AppState, req: EditorSaveRequest, id: u64) -> Result<(), ApiError> {
    let (parent, name, file) = editor_parent_and_file(state, &req.root, &req.path, &req.name)?;
    let metadata = can_replace_text_file(&file)?;
    let content = bounded_read(&file)?;
    if content != req.original.as_bytes() {
        return Err(ApiError::conflict("Soubor se mezitím změnil. Obnov editor a znovu zkontroluj obsah; nebylo nic přepsáno."));
    }
    let (mut partial, guard) = make_temp_upload(parent.try_clone()
        .map_err(|e| io_error("Otevření adresáře", e))?, id)
        .map_err(|e| io_error("Příprava souboru", e))?;
    partial.write_all(req.text.as_bytes())
        .map_err(|e| io_error("Zápis textu", e))?;
    // A directory's default ACL may be inherited by the new file. Avoid
    // quietly changing an original that did not have such attributes.
    // SAFETY: size=0 with a null output pointer is the xattr-list length query.
    let temp_xattrs = unsafe { libc::flistxattr(partial.as_raw_fd(), std::ptr::null_mut(), 0) };
    if temp_xattrs < 0 {
        return Err(io_error("Kontrola atributů dočasného souboru", io::Error::last_os_error()));
    }
    if temp_xattrs > 0 {
        return Err(ApiError::forbidden("Adresář předává souborům ACL/xattr. Uložení by změnilo metadata; použij externí editor."));
    }
    // New inode must retain original owner and permissions. Any failure aborts.
    let temp_meta = partial.metadata().map_err(|e| io_error("Metadata dočasného souboru", e))?;
    if (temp_meta.uid(), temp_meta.gid()) != (metadata.uid(), metadata.gid()) {
        // SAFETY: owned writable file descriptor; fchown uses trusted original metadata.
        if unsafe { libc::fchown(partial.as_raw_fd(), metadata.uid(), metadata.gid()) } != 0 {
            return Err(io_error("Zachování vlastníka", io::Error::last_os_error()));
        }
    }
    // SAFETY: fchmod receives a valid fd and the original POSIX mode bits.
    if unsafe { libc::fchmod(partial.as_raw_fd(), (metadata.mode() & 0o7777) as libc::mode_t) } != 0 {
        return Err(io_error("Zachování oprávnění", io::Error::last_os_error()));
    }
    partial.sync_all().map_err(|e| io_error("Synchronizace textu", e))?;
    drop(partial);

    // Best-effort conflict detection for other processes. Application writes
    // are serialized, but a separate process could still race after this check.
    let check_file = checked_openat2(parent.as_raw_fd(), &name,
        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .map_err(|e| io_error("Kontrola změn souboru", e))?;
    let check_meta = check_file.metadata().map_err(|e| io_error("Kontrola souboru", e))?;
    if !same_file(&metadata, &check_meta) || bounded_read(&check_file)? != req.original.as_bytes() {
        return Err(ApiError::conflict("Soubor mezitím upravil jiný proces. Nový obsah nebyl uložen."));
    }
    guard.commit(&req.name, UploadMode::Overwrite)
        .map_err(|e| io_error("Dokončení uložení", e))?;
    // fsync parent for directory-entry durability (best effort; already committed).
    let _ = parent.sync_all();
    Ok(())
}

/// POST /api/editor/save (application/json); writing is disabled by default.
pub(super) async fn save_text(
    State(state): State<AppState>, headers: HeaderMap,
    Json(req): Json<EditorSaveRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_write_header(&headers, "edit")?;
    if !valid_entry_name(&req.name) {
        return Err(ApiError::bad_request("Neplatný název textového souboru."));
    }
    validate_editor_text(req.original.as_bytes().to_vec())?;
    validate_editor_text(req.text.as_bytes().to_vec())?;
    let _permit = state.write_permit()?;
    let id = state.next_job_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    tokio::task::spawn_blocking(move || save_text_blocking(&state, req, id))
        .await.map_err(|_| ApiError::bad_request("Uložení textu selhalo."))??;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(json!({"ok": true}))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use axum::body::to_bytes;
    use std::os::unix::fs::symlink;
    use std::sync::{Arc, Mutex};
    use std::sync::atomic::{AtomicBool, AtomicU64};

    fn state(data: &Path, models: &Path, max_upload_bytes: u64) -> AppState {
        AppState {
            data: data.to_path_buf(), models: models.to_path_buf(),
            data_fd: Arc::new(fs::File::open(data).unwrap()),
            models_fd: Arc::new(fs::File::open(models).unwrap()),
            writes_enabled: true,
            unrestricted: false,
            admin_password: None,
            max_upload_bytes,
            job: Arc::new(Mutex::new(None)),
            next_job_id: Arc::new(AtomicU64::new(1)),
            write_busy: Arc::new(AtomicBool::new(false)),
        }
    }

    #[tokio::test]
    async fn create_file_is_exclusive_confined_and_respects_read_only() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let models = tmp.path().join("models");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&models).unwrap();
        fs::create_dir(data.join("sub")).unwrap();
        fs::write(tmp.path().join("outside.txt"), b"original").unwrap();
        symlink(tmp.path(), data.join("escape")).unwrap();
        let app = state(&data, &models, 1024);
        let mut headers = HeaderMap::new();
        headers.insert("x-salamander-request", HeaderValue::from_static("create-file"));
        let req = CreateFileRequest {
            root: "data".into(), path: "sub".into(), name: "novy.txt".into(),
        };
        let created = create_empty_file(State(app.clone()), headers.clone(), Json(CreateFileRequest {
            root: req.root.clone(), path: req.path.clone(), name: req.name.clone(),
        })).await;
        assert!(created.is_ok());
        assert_eq!(fs::metadata(data.join("sub/novy.txt")).unwrap().len(), 0);

        // A second request is a conflict; it must not truncate the original.
        fs::write(data.join("sub/novy.txt"), b"keep me").unwrap();
        let duplicate = create_empty_file(State(app.clone()), headers.clone(), Json(CreateFileRequest {
            root: req.root.clone(), path: req.path.clone(), name: req.name.clone(),
        })).await;
        assert_eq!(duplicate.err().unwrap().status, StatusCode::CONFLICT);
        assert_eq!(fs::read(data.join("sub/novy.txt")).unwrap(), b"keep me");

        // Cannot traverse a parent directory, or a symlink to outside.
        for path in ["../", "escape"] {
            let denied = create_empty_file(State(app.clone()), headers.clone(), Json(CreateFileRequest {
                root: "data".into(), path: path.into(), name: "hacked.txt".into(),
            })).await;
            assert_eq!(denied.err().unwrap().status, StatusCode::FORBIDDEN);
        }
        assert!(!tmp.path().join("hacked.txt").exists());
        let invalid_name = create_empty_file(State(app.clone()), headers.clone(), Json(CreateFileRequest {
            root: "models".into(), path: "".into(), name: "../evil".into(),
        })).await;
        assert_eq!(invalid_name.err().unwrap().status, StatusCode::BAD_REQUEST);

        // Read-only mode remains read-only despite a valid request header.
        let mut read_only = app;
        read_only.writes_enabled = false;
        let denied = create_empty_file(State(read_only), headers, Json(CreateFileRequest {
            root: "models".into(), path: "".into(), name: "forbidden.txt".into(),
        })).await;
        assert_eq!(denied.err().unwrap().status, StatusCode::FORBIDDEN);
        assert!(!models.join("forbidden.txt").exists());
    }

    #[test]
    fn rejects_symlinks_and_parent_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let other = tmp.path().join("other");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&other).unwrap();
        fs::write(other.join("secret"), b"no").unwrap();
        symlink(&other, data.join("escape")).unwrap();
        symlink(other.join("secret"), data.join("secret-link")).unwrap();
        let root = File::open(&data).unwrap();
        assert!(open_relative_directory(&root, "../other").is_err());
        assert!(open_relative_directory(&root, "/etc").is_err());
        assert!(open_relative_directory(&root, "escape").is_err());
        let path = CString::new("secret-link").unwrap();
        assert!(checked_openat2(root.as_raw_fd(), &path, libc::O_RDONLY | libc::O_CLOEXEC).is_err());
    }

    #[test]
    fn temp_upload_does_not_overwrite_and_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("existing.txt"), b"old").unwrap();
        let dir = File::open(tmp.path()).unwrap();
        let (mut partial, guard) = make_temp_upload(dir.try_clone().unwrap(), 123).unwrap();
        use std::io::Write;
        partial.write_all(b"new").unwrap();
        drop(partial);
        assert!(guard.commit("existing.txt", UploadMode::Reject).is_err());
        assert_eq!(fs::read(tmp.path().join("existing.txt")).unwrap(), b"old");
        assert_eq!(fs::read_dir(tmp.path()).unwrap().count(), 1);
        let (mut partial, guard) = make_temp_upload(dir, 124).unwrap();
        partial.write_all(b"new").unwrap();
        drop(partial);
        guard.commit("new.txt", UploadMode::Reject).unwrap();
        assert_eq!(fs::read(tmp.path().join("new.txt")).unwrap(), b"new");
    }

    #[tokio::test]
    async fn upload_and_download_stream_without_overwriting() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let models = tmp.path().join("models");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&models).unwrap();
        fs::create_dir(data.join("inside")).unwrap();
        let state = state(&data, &models, 1024);
        let query = || TransferQuery {
            root: "data".to_owned(), path: "inside".to_owned(), name: "pokus-č.txt".to_owned(),
            mode: None, new_name: None,
        };
        let request = || Request::builder()
            .method("POST").uri("/api/upload")
            .header("x-salamander-request", "upload")
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from("Ahoj světe!".as_bytes().to_vec())).unwrap();
        let response = upload_file(State(state.clone()), Query(query()), request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(fs::read(data.join("inside/pokus-č.txt")).unwrap(), "Ahoj světe!".as_bytes());
        let error = upload_file(State(state.clone()), Query(query()), request()).await.err().expect("upload musí skončit konfliktem");
        assert_eq!(error.status, StatusCode::CONFLICT);
        let response = download_file(State(state), Query(query()), HeaderMap::new()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 1024).await.unwrap();
        assert_eq!(bytes.as_ref(), "Ahoj světe!".as_bytes());
    }

    #[tokio::test]
    async fn upload_limit_removes_partial_file() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        fs::create_dir(&data).unwrap();
        let state = state(&data, &data, 2);
        let req = Request::builder()
            .method("POST").uri("/api/upload")
            .header("x-salamander-request", "upload")
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from("too much")).unwrap();
        let error = upload_file(State(state), Query(TransferQuery {
            root: "data".to_owned(), path: "".to_owned(), name: "too-large.txt".to_owned(),
            mode: None, new_name: None,
        }), req).await.err().expect("velký upload musí být odmítnut");
        assert_eq!(error.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(fs::read_dir(&data).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn upload_conflicts_allow_explicit_overwrite_or_new_name() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("old.txt"), b"original").unwrap();
        let state = state(&data, &data, 1024);
        let query = |mode: &str, rename: Option<&str>| TransferQuery {
            root: "data".into(), path: "".into(), name: "old.txt".into(),
            mode: Some(mode.into()), new_name: rename.map(str::to_owned),
        };
        let request = || Request::builder()
            .method("POST").uri("/api/upload")
            .header("x-salamander-request", "upload")
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .body(Body::from("replacement")).unwrap();

        let result = upload_file(State(state.clone()), Query(query("reject", None)), request()).await;
        assert_eq!(result.err().unwrap().status, StatusCode::CONFLICT);
        assert_eq!(fs::read(data.join("old.txt")).unwrap(), b"original");

        let response = upload_file(State(state.clone()), Query(query("rename", Some("new.txt"))), request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(fs::read(data.join("new.txt")).unwrap(), b"replacement");
        assert_eq!(fs::read(data.join("old.txt")).unwrap(), b"original");

        let response = upload_file(State(state), Query(query("overwrite", None)), request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(fs::read(data.join("old.txt")).unwrap(), b"replacement");
        assert_eq!(fs::read_dir(&data).unwrap().count(), 2);
    }

    #[tokio::test]
    async fn editor_reads_and_saves_utf8_without_losing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("poznámka.txt"), "Původní řádek\n").unwrap();
        fs::set_permissions(data.join("poznámka.txt"), fs::Permissions::from_mode(0o640)).unwrap();
        let state = state(&data, &data, 1024);
        let query = EditorQuery {
            root: "data".into(), path: "".into(), name: "poznámka.txt".into(),
        };
        let response = open_text(State(state.clone()), Query(query)).await.unwrap().into_response();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["text"], "Původní řádek\n");
        assert_eq!(value["read_only"], false);

        let mut headers = HeaderMap::new();
        headers.insert("x-salamander-request", HeaderValue::from_static("edit"));
        let updated = EditorSaveRequest {
            root: "data".into(), path: "".into(), name: "poznámka.txt".into(),
            original: "Původní řádek\n".into(), text: "Upravený text\n".into(),
        };
        let response = save_text(State(state.clone()), headers.clone(), Json(updated)).await.unwrap().into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(fs::read_to_string(data.join("poznámka.txt")).unwrap(), "Upravený text\n");
        assert_eq!(fs::metadata(data.join("poznámka.txt")).unwrap().permissions().mode() & 0o777, 0o640);

        let stale = EditorSaveRequest {
            root: "data".into(), path: "".into(), name: "poznámka.txt".into(),
            original: "Původní řádek\n".into(), text: "Nesprávný obsah".into(),
        };
        let error = save_text(State(state), headers, Json(stale)).await.err().unwrap();
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(fs::read_to_string(data.join("poznámka.txt")).unwrap(), "Upravený text\n");
    }

    #[tokio::test]
    async fn editor_rejects_binary_symlinks_and_hardlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::write(data.join("binary.dat"), b"abc\x00def").unwrap();
        fs::write(data.join("original.txt"), b"safe").unwrap();
        fs::hard_link(data.join("original.txt"), data.join("hardlink.txt")).unwrap();
        symlink(data.join("original.txt"), data.join("symlink.txt")).unwrap();
        let state = state(&data, &data, 1024);
        for name in ["binary.dat", "symlink.txt"] {
            let result = open_text(State(state.clone()), Query(EditorQuery {
                root: "data".into(), path: "".into(), name: name.into(),
            })).await;
            assert!(result.is_err(), "Editor must reject {name}");
        }
        let mut headers = HeaderMap::new();
        headers.insert("x-salamander-request", HeaderValue::from_static("edit"));
        let result = save_text(State(state), headers, Json(EditorSaveRequest {
            root: "data".into(), path: "".into(), name: "original.txt".into(),
            original: "safe".into(), text: "change".into(),
        })).await;
        assert_eq!(result.err().unwrap().status, StatusCode::FORBIDDEN);
        assert_eq!(fs::read(data.join("original.txt")).unwrap(), b"safe");
    }

    #[tokio::test]
    async fn upload_refuses_overwriting_symlinks_and_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        fs::create_dir(&data).unwrap();
        fs::create_dir(data.join("dir")) .unwrap();
        fs::write(tmp.path().join("outside.txt"), b"safe").unwrap();
        symlink(tmp.path().join("outside.txt"), data.join("link.txt")).unwrap();
        let state = state(&data, &data, 1024);
        for name in ["link.txt", "dir"] {
            let req = Request::builder().method("POST").uri("/api/upload")
                .header("x-salamander-request", "upload")
                .header(header::CONTENT_TYPE, "application/octet-stream")
                .body(Body::from("malicious")).unwrap();
            let result = upload_file(State(state.clone()), Query(TransferQuery {
                root: "data".into(), path: String::new(), name: name.into(),
                mode: Some("overwrite".into()), new_name: None,
            }), req).await;
            assert_eq!(result.err().unwrap().status, StatusCode::FORBIDDEN);
        }
        assert_eq!(fs::read(tmp.path().join("outside.txt")).unwrap(), b"safe");
        assert!(data.join("dir").is_dir());
    }
}

#[cfg(test)]
mod resume_range_tests {
    use super::*;
    #[test]
    fn ranges_are_bounded_and_single() {
        assert_eq!(parse_single_range("bytes=5-9", 20), Some((5,9)));
        assert_eq!(parse_single_range("bytes=5-", 20), Some((5,19)));
        assert_eq!(parse_single_range("bytes=-4", 20), Some((16,19)));
        assert_eq!(parse_single_range("bytes=0-999", 20), Some((0,19)));
        for bad in ["bytes=20-", "bytes=10-9", "bytes=0-1,3-4", "bytes=-0", "items=0-1"] {
            assert_eq!(parse_single_range(bad, 20), None);
        }
        assert_eq!(parse_single_range("bytes=0-", 0), None);
    }
}
