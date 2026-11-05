//! Durable, resumable uploads. Each upload is a .part file plus a small atomic
//! .json checkpoint in the TARGET directory so final rename is same-filesystem.
//! The browser must reselect the local File after a full browser restart.
//! There is no separate user identity in this standalone prototype; run only
//! on loopback until the platform authentication boundary is implemented.
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd};

use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::Json;
use axum::response::IntoResponse;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use super::{ApiError, AppState, require_write_header, valid_entry_name};
use super::transfer_http::{existing_kind, io_error, open_relative_directory, RENAME_NOREPLACE};

const CHUNK_LIMIT: u64 = 8 * 1024 * 1024;
const META_LIMIT: u64 = 8 * 1024;

#[derive(Serialize, Deserialize, Clone, Debug)]
struct UploadMeta {
    id: String,
    name: String,
    target_name: String,
    expected_size: u64,
    last_modified: i64,
    fingerprint: String,
    mode: String,
    committed: u64,
}

#[derive(Deserialize)]
pub(super) struct StartRequest {
    root: String,
    #[serde(default)] path: String,
    name: String,
    expected_size: u64,
    last_modified: i64,
    fingerprint: String,
    #[serde(default)] mode: Option<String>,
    #[serde(default)] new_name: Option<String>,
}
#[derive(Deserialize)]
pub(super) struct SessionQuery {
    root: String,
    #[serde(default)] path: String,
    id: String,
}
#[derive(Deserialize)]
pub(super) struct ChunkQuery {
    root: String,
    #[serde(default)] path: String,
    id: String,
    offset: u64,
}
#[derive(Deserialize)]
pub(super) struct SessionRequest {
    root: String,
    #[serde(default)] path: String,
    id: String,
}
fn id_ok(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
fn fingerprint_ok(fingerprint: &str) -> bool {
    fingerprint.len() == 64 && fingerprint.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}
fn names(id: &str) -> Result<(CString, CString, CString), ApiError> {
    if !id_ok(id) { return Err(ApiError::bad_request("Neplatné ID přenosu.")); }
    Ok((
        CString::new(format!(".salamander-resume-{id}.part")).unwrap(),
        CString::new(format!(".salamander-resume-{id}.json")).unwrap(),
        CString::new(format!(".salamander-resume-{id}.json.next")).unwrap(),
    ))
}
fn open_at(parent: &File, name: &CString, flags: i32, mode: libc::mode_t) -> io::Result<File> {
    // SAFETY: parent is an open directory, and name is a single NUL-free component.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(),
        flags | libc::O_NOFOLLOW | libc::O_CLOEXEC, mode) };
    if fd < 0 { Err(io::Error::last_os_error()) }
    else { Ok(unsafe { File::from_raw_fd(fd) }) }
}
fn unlink(parent: &File, name: &CString) -> io::Result<()> {
    if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) } == 0 { return Ok(()); }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::NotFound { Ok(()) } else { Err(err) }
}
fn rename(parent: &File, from: &CString, to: &CString, replace: bool) -> io::Result<()> {
    let result = if replace {
        unsafe { libc::renameat(parent.as_raw_fd(), from.as_ptr(), parent.as_raw_fd(), to.as_ptr()) as libc::c_long }
    } else {
        unsafe { libc::syscall(libc::SYS_renameat2, parent.as_raw_fd(), from.as_ptr(),
            parent.as_raw_fd(), to.as_ptr(), RENAME_NOREPLACE) }
    };
    if result < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
fn random_id() -> Result<String, ApiError> {
    let mut bytes = [0u8; 16];
    // getrandom has no filesystem dependency and works even under Landlock.
    let count = unsafe { libc::syscall(libc::SYS_getrandom, bytes.as_mut_ptr(), bytes.len(), 0) };
    if count != bytes.len() as libc::c_long {
        return Err(ApiError::bad_request("Systém nemohl vytvořit náhodné ID přenosu."));
    }
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
}
fn write_meta(parent: &File, meta: &UploadMeta) -> Result<(), ApiError> {
    let (_, target, next) = names(&meta.id)?;
    let bytes = serde_json::to_vec(meta).map_err(|_| ApiError::bad_request("Metadata přenosu nelze uložit."))?;
    if bytes.len() > META_LIMIT as usize { return Err(ApiError::bad_request("Metadata přenosu jsou příliš dlouhá.")); }
    unlink(parent, &next).map_err(|e| io_error("Příprava metadat přenosu", e))?;
    let mut file = open_at(parent, &next, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL, 0o600)
        .map_err(|e| io_error("Vytvoření metadat", e))?;
    if let Err(e) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        let _ = unlink(parent, &next);
        return Err(io_error("Uložení metadat", e));
    }
    drop(file);
    rename(parent, &next, &target, true).map_err(|e| io_error("Potvrzení metadat", e))?;
    parent.sync_all().map_err(|e| io_error("Potvrzení adresáře", e))?;
    Ok(())
}
fn load_meta(parent: &File, id: &str, max_bytes: u64) -> Result<UploadMeta, ApiError> {
    let (_, meta_name, _) = names(id)?;
    let file = open_at(parent, &meta_name, libc::O_RDONLY, 0)
        .map_err(|e| io_error("Otevření rozpracovaného přenosu", e))?;
    if !file.metadata().map_err(|e| io_error("Metadata přenosu", e))?.is_file() {
        return Err(ApiError::forbidden("Metadata přenosu nejsou běžný soubor."));
    }
    let mut data = Vec::new();
    (&file).take(META_LIMIT + 1).read_to_end(&mut data)
        .map_err(|e| io_error("Čtení metadat", e))?;
    if data.len() as u64 > META_LIMIT { return Err(ApiError::forbidden("Přenos obsahuje neplatná metadata.")); }
    let meta: UploadMeta = serde_json::from_slice(&data)
        .map_err(|_| ApiError::conflict("Záznam rozpracovaného přenosu je poškozený."))?;
    if meta.id != id || !id_ok(&meta.id) || !valid_entry_name(&meta.name)
        || !valid_entry_name(&meta.target_name) || !fingerprint_ok(&meta.fingerprint)
        || meta.expected_size > max_bytes || meta.committed > meta.expected_size
        || !matches!(meta.mode.as_str(), "reject" | "overwrite" | "rename") {
        return Err(ApiError::conflict("Metadata rozpracovaného přenosu neprošla kontrolou."));
    }
    Ok(meta)
}
fn open_part(parent: &File, meta: &UploadMeta, writable: bool) -> Result<File, ApiError> {
    let (part, _, _) = names(&meta.id)?;
    let file = open_at(parent, &part, if writable { libc::O_RDWR } else { libc::O_RDONLY }, 0)
        .map_err(|e| io_error("Otevření části souboru", e))?;
    let st = file.metadata().map_err(|e| io_error("Kontrola části souboru", e))?;
    if !st.is_file() || st.len() < meta.committed || st.len() > meta.expected_size {
        return Err(ApiError::conflict("Rozpracovaný soubor má nesprávnou délku."));
    }
    Ok(file)
}
fn parent_for(state: &AppState, root: &str, path: &str) -> Result<File, ApiError> {
    open_relative_directory(state.root_handle(root)?, path)
}

/// POST JSON: root, path, name, expected_size, last_modified, fingerprint,
/// mode, new_name. A 409 is returned BEFORE creating a session if target exists.
pub(super) async fn start(
    State(state): State<AppState>, headers: HeaderMap, Json(req): Json<StartRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_write_header(&headers, "resume-start")?;
    if !valid_entry_name(&req.name) || !fingerprint_ok(&req.fingerprint) {
        return Err(ApiError::bad_request("Neplatný název nebo kontrolní otisk souboru."));
    }
    if req.expected_size > state.max_upload_bytes {
        return Err(ApiError { status: StatusCode::PAYLOAD_TOO_LARGE,
            message: "Soubor překračuje limit přenosu.".into() });
    }
    let mode = req.mode.as_deref().unwrap_or("reject").to_owned();
    if !matches!(mode.as_str(), "reject" | "overwrite" | "rename") {
        return Err(ApiError::bad_request("Neznámý způsob řešení konfliktu."));
    }
    let name = if mode == "rename" || (mode == "overwrite" && req.new_name.is_some()) {
        let n = req.new_name.as_deref().ok_or_else(|| ApiError::bad_request("Chybí nový název."))?;
        if !valid_entry_name(n) || (mode == "rename" && n == req.name) {
            return Err(ApiError::bad_request("Neplatný nový název."));
        }
        n.to_owned()
    } else { req.name.clone() };
    let id = random_id()?;
    let meta = UploadMeta { id: id.clone(), name:req.name, target_name:name,
        expected_size:req.expected_size, last_modified:req.last_modified,
        fingerprint:req.fingerprint, mode:mode.clone(), committed:0 };
    let _permit = state.write_permit()?;
    tokio::task::spawn_blocking(move || {
        let parent = parent_for(&state, &req.root, &req.path)?;
        let target = CString::new(meta.target_name.as_str()).unwrap();
        let existing = existing_kind(&parent, &target).map_err(|e| io_error("Kontrola cíle", e))?;
        if mode != "overwrite" && existing.is_some() {
            return Err(ApiError::conflict("Cílový soubor již existuje."));
        }
        if mode == "overwrite" && existing.is_some_and(|k| k != libc::S_IFREG) {
            return Err(ApiError::forbidden("Přepsat lze pouze obyčejný soubor."));
        }
        let (part, _, _) = names(&meta.id)?;
        let file = open_at(&parent, &part, libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600)
            .map_err(|e| io_error("Vytvoření rozpracovaného souboru", e))?;
        file.sync_all().map_err(|e| io_error("Příprava souboru", e))?;
        drop(file);
        if let Err(err) = write_meta(&parent, &meta) {
            let _ = unlink(&parent, &part);
            return Err(err);
        }
        Ok::<_, ApiError>(meta)
    }).await.map_err(|_| ApiError::bad_request("Příprava přenosu selhala."))?
        .map(|meta| (StatusCode::CREATED, Json(meta)))
}

/// GET a durable server checkpoint; the client also verifies name, size,
/// lastModified and a SHA-256 fingerprint of the file's ends before resuming.
pub(super) async fn status(
    State(state): State<AppState>, Query(q): Query<SessionQuery>,
) -> Result<impl IntoResponse, ApiError> {
    tokio::task::spawn_blocking(move || {
        let parent = parent_for(&state, &q.root, &q.path)?;
        let meta = load_meta(&parent, &q.id, state.max_upload_bytes)?;
        let _part = open_part(&parent, &meta, false)?;
        Ok::<_, ApiError>(([(header::CACHE_CONTROL, "no-store")], Json(meta)))
    }).await.map_err(|_| ApiError::bad_request("Kontrola přenosu selhala."))?
}

/// POST /api/resumable/chunk?root=...&path=...&id=...&offset=N
/// The server will accept <=8 MiB, sync data, and only then advance the
/// durable checkpoint. A failed/incomplete chunk never advances its offset.
pub(super) async fn chunk(
    State(state): State<AppState>, Query(q): Query<ChunkQuery>, request: Request,
) -> Result<impl IntoResponse, ApiError> {
    require_write_header(request.headers(), "resume-chunk")?;
    if request.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok())
        != Some("application/octet-stream") {
        return Err(ApiError::bad_request("Blok vyžaduje application/octet-stream."));
    }
    if request.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok()).is_some_and(|n| n > CHUNK_LIMIT) {
        return Err(ApiError { status:StatusCode::PAYLOAD_TOO_LARGE,
            message:"Blok přesahuje 8 MiB.".into() });
    }
    let _permit = state.write_permit()?;
    let (file, mut meta) = tokio::task::spawn_blocking({
        let state = state.clone();
        let q_root = q.root.clone(); let q_path = q.path.clone(); let q_id = q.id.clone();
        let expected_offset = q.offset;
        move || {
            let parent = parent_for(&state, &q_root, &q_path)?;
            let meta = load_meta(&parent, &q_id, state.max_upload_bytes)?;
            if meta.committed != expected_offset {
                return Err(ApiError::conflict("Pozice přenosu nesouhlasí. Obnov stav přenosu."));
            }
            let file = open_part(&parent, &meta, true)?;
            Ok::<_, ApiError>((file, meta))
        }
    }).await.map_err(|_| ApiError::bad_request("Příprava bloku selhala."))??;
    let mut file = tokio::fs::File::from_std(file);
    // Discard bytes left by an interrupted prior attempt or process crash.
    file.set_len(meta.committed).await.map_err(|e| io_error("Obnova délky přenosu", e))?;
    file.seek(SeekFrom::Start(meta.committed)).await
        .map_err(|e| io_error("Nastavení pozice", e))?;
    let mut sent = 0u64;
    let mut stream = request.into_body().into_data_stream();
    while let Some(next) = stream.next().await {
        let bytes = next.map_err(|_| ApiError::bad_request("Spojení se během bloku přerušilo."))?;
        sent = sent.checked_add(bytes.len() as u64)
            .ok_or_else(|| ApiError::bad_request("Neplatná délka bloku."))?;
        if sent > CHUNK_LIMIT || sent > meta.expected_size - meta.committed {
            return Err(ApiError { status:StatusCode::PAYLOAD_TOO_LARGE,
                message:"Blok překračuje povolenou velikost.".into() });
        }
        file.write_all(&bytes).await.map_err(|e| io_error("Uložení bloku", e))?;
    }
    if sent == 0 && meta.committed < meta.expected_size {
        return Err(ApiError::bad_request("Prázdný blok není platný."));
    }
    file.sync_data().await.map_err(|e| io_error("Potvrzení bloku na disku", e))?;
    drop(file);
    meta.committed += sent;
    let written = meta.committed;
    tokio::task::spawn_blocking(move || {
        let parent = parent_for(&state, &q.root, &q.path)?;
        write_meta(&parent, &meta)
    }).await.map_err(|_| ApiError::bad_request("Potvrzení bloku selhalo."))??;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(json!({"offset":written}))))
}

pub(super) async fn finish(
    State(state): State<AppState>, headers: HeaderMap, Json(q): Json<SessionRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_write_header(&headers, "resume-finish")?;
    let _permit = state.write_permit()?;
    let max_bytes = state.max_upload_bytes;
    tokio::task::spawn_blocking(move || {
        let parent = parent_for(&state, &q.root, &q.path)?;
        let meta = load_meta(&parent, &q.id, max_bytes)?;
        if meta.committed != meta.expected_size {
            return Err(ApiError::conflict("Přenos ještě není dokončen."));
        }
        let file = open_part(&parent, &meta, false)?;
        if file.metadata().map_err(|e| io_error("Kontrola souboru", e))?.len() != meta.expected_size {
            return Err(ApiError::conflict("Část souboru nemá očekávanou velikost."));
        }
        file.sync_all().map_err(|e| io_error("Uložení souboru", e))?;
        drop(file);
        let (part, json_name, next) = names(&meta.id)?;
        let target = CString::new(meta.target_name.as_str()).unwrap();
        let current = existing_kind(&parent, &target).map_err(|e| io_error("Kontrola cíle", e))?;
        if meta.mode == "overwrite" && current.is_some_and(|k| k != libc::S_IFREG) {
            return Err(ApiError::forbidden("Přepsat lze pouze běžný soubor."));
        }
        rename(&parent, &part, &target, meta.mode == "overwrite")
            .map_err(|e| io_error("Dokončení přenosu", e))?;
        let _ = unlink(&parent, &json_name);
        let _ = unlink(&parent, &next);
        let _ = parent.sync_all();
        Ok::<_, ApiError>((StatusCode::CREATED, Json(json!({
            "ok":true,"name":meta.target_name,"bytes":meta.expected_size
        }))))
    }).await.map_err(|_| ApiError::bad_request("Dokončení přenosu selhalo."))?
}

/// Explicit cleanup; interrupted sessions are otherwise retained for resume.
pub(super) async fn cancel(
    State(state): State<AppState>, headers: HeaderMap, Json(q): Json<SessionRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_write_header(&headers, "resume-cancel")?;
    let _permit = state.write_permit()?;
    tokio::task::spawn_blocking(move || {
        let parent = parent_for(&state, &q.root, &q.path)?;
        let meta = load_meta(&parent, &q.id, state.max_upload_bytes)?;
        let (part, json_name, next) = names(&meta.id)?;
        unlink(&parent, &part).map_err(|e| io_error("Smazání části souboru", e))?;
        unlink(&parent, &json_name).map_err(|e| io_error("Smazání záznamu přenosu", e))?;
        let _ = unlink(&parent, &next);
        let _ = parent.sync_all();
        Ok::<_, ApiError>(([(header::CACHE_CONTROL, "no-store")], Json(json!({"ok":true}))))
    }).await.map_err(|_| ApiError::bad_request("Zrušení přenosu selhalo."))?
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_names_are_confined() {
        let id = "00000000000000000000000000000000";
        let (part, meta, _) = names(id).unwrap();
        assert_eq!(part.to_str().unwrap(), ".salamander-resume-00000000000000000000000000000000.part");
        assert!(meta.to_str().unwrap().ends_with(".json"));
        for bad in ["../bad", "ABCDEF00000000000000000000000000", "1234", "00/00"] {
            assert!(names(bad).is_err());
        }
    }
    #[test]
    fn crash_checkpoint_keeps_only_committed_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let parent = File::open(dir.path()).unwrap();
        let id = "0123456789abcdef0123456789abcdef";
        let meta = UploadMeta {id:id.into(), name:"x.bin".into(), target_name:"x.bin".into(),
            expected_size:123, last_modified:0, fingerprint:"f".repeat(64),
            mode:"reject".into(), committed:9};
        let (part, _, _) = names(id).unwrap();
        let mut file = open_at(&parent, &part, libc::O_CREAT|libc::O_EXCL|libc::O_RDWR, 0o600).unwrap();
        file.write_all(&[0u8; 20]).unwrap();
        write_meta(&parent, &meta).unwrap();
        let decoded = load_meta(&parent, id, 123).unwrap();
        assert_eq!(decoded.committed, 9);
        assert_eq!(open_part(&parent, &decoded, true).unwrap().metadata().unwrap().len(), 20);
    }
}
