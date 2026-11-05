//! Salamander RS Web — dual-pane filesystem prototype.
//! Default: confined local mode. Explicit --unrestricted: local administrator
//! mode with mandatory HTTP Basic authentication; NEVER expose it on a network.
//! Reads, transfers and ZIP archives use salamander-core.
//! Landlock in writable mode adds kernel-enforced filesystem restrictions,
//! but pathname-based operations are NOT production-ready under races.

mod security;
mod transfer_http;
mod resumable;
mod preview;
mod auth;

use std::collections::HashSet;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::UNIX_EPOCH;

use axum::extract::{Query, Request, State};
use axum::middleware::{self, Next};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use salamander_core::archive::{self, ExtractRequest, PackFormat};
use salamander_core::model::{self, EntryKind};
use salamander_core::ops::{self, ConflictAction, Op, OpEvent, OpKind, Outcome};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Debug)]
struct AppState {
    data: PathBuf,
    models: PathBuf,
    /// Pinned root directory descriptors for safe upload/download traversals.
    data_fd: Arc<fs::File>,
    models_fd: Arc<fs::File>,
    /// Upload cap (per file), including requests sent without Content-Length.
    max_upload_bytes: u64,
    /// Default: read-only; opt-in writes are confined by Landlock to both roots.
    /// --unrestricted disables Landlock and requires HTTP Basic authentication.
    writes_enabled: bool,
    unrestricted: bool,
    admin_password: Option<Arc<auth::AdminPassword>>,
    job: Arc<Mutex<Option<CopyStatus>>>,
    next_job_id: Arc<AtomicU64>,
    /// All write requests, including background transfers and ZIP jobs, are serialized.
    write_busy: Arc<AtomicBool>,
}

impl AppState {
    fn from_env(unrestricted: bool) -> Result<Self, String> {
        let default_data = if unrestricted { "/" } else { "/data" };
        let default_models = if unrestricted { "/" } else { "/models" };
        let data = canonical_root("SALAMANDER_DATA_ROOT", default_data)?;
        let models = canonical_root("SALAMANDER_MODELS_ROOT", default_models)?;
        let writes_enabled = unrestricted || std::env::var("SALAMANDER_ENABLE_WRITES")
            .is_ok_and(|value| value == "1");
        // The two roots can be anywhere (including bind-mounted /data and
        // /models), but a confined instance must never grant all of "/".
        // This check is independent of the old development-only /tmp guard.
        if !unrestricted {
            validate_confined_roots(&data, &models)?;
            if writes_enabled && (unsafe { libc::geteuid() } == 0) {
                return Err("Zabezpečený zápisový režim nespouštěj jako root; nastav běžné UID/GID (v Dockeru --user). Pro správu celého serveru je oddělený režim --unrestricted.".into());
            }
        }
        let data_fd = Arc::new(fs::File::open(&data)
            .map_err(|e| format!("Otevření DATA kořene: {e}"))?);
        let models_fd = Arc::new(fs::File::open(&models)
            .map_err(|e| format!("Otevření MODELS kořene: {e}"))?);
        let max_upload_bytes = std::env::var("SALAMANDER_MAX_UPLOAD_BYTES")
            .unwrap_or_else(|_| (256u64 * 1024 * 1024 * 1024).to_string())
            .parse::<u64>()
            .map_err(|e| format!("SALAMANDER_MAX_UPLOAD_BYTES: {e}"))?;
        if max_upload_bytes == 0 {
            return Err("SALAMANDER_MAX_UPLOAD_BYTES musí být kladné číslo.".into());
        }
        let admin_password = if unrestricted {
            Some(Arc::new(auth::AdminPassword::from_env()?))
        } else {
            None
        };
        Ok(Self {
            data,
            models,
            data_fd,
            models_fd,
            max_upload_bytes,
            writes_enabled,
            unrestricted,
            admin_password,
            job: Arc::new(Mutex::new(None)),
            next_job_id: Arc::new(AtomicU64::new(1)),
            write_busy: Arc::new(AtomicBool::new(false)),
        })
    }

    fn root(&self, name: &str) -> Result<&Path, ApiError> {
        match name {
            "data" => Ok(&self.data),
            "models" => Ok(&self.models),
            _ => Err(ApiError::bad_request("Neznámý strom.")),
        }
    }

    fn root_handle(&self, name: &str) -> Result<&fs::File, ApiError> {
        match name {
            "data" => Ok(&self.data_fd),
            "models" => Ok(&self.models_fd),
            _ => Err(ApiError::bad_request("Neznámý strom.")),
        }
    }

    fn write_permit(&self) -> Result<WritePermit, ApiError> {
        if !self.writes_enabled {
            return Err(ApiError::forbidden("Zápis je vypnutý. Pro zápis v omezených kořenech nastav SALAMANDER_ENABLE_WRITES=1."));
        }
        if self.write_busy.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(ApiError::conflict("Jiná souborová operace už probíhá."));
        }
        Ok(WritePermit(Arc::clone(&self.write_busy)))
    }
}

struct WritePermit(Arc<AtomicBool>);
impl Drop for WritePermit {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Confined mode accepts arbitrary configured directories, but never the
/// entire host filesystem as either root. To browse "/", use the explicit
/// local administrator mode (--unrestricted) with authentication instead.
fn validate_confined_roots(data: &Path, models: &Path) -> Result<(), String> {
    if data == Path::new("/") || models == Path::new("/") {
        return Err("V zabezpečeném režimu nesmí být kořenem '/'. Nastav dva uživatelské adresáře, nebo pro lokální administraci zvol --unrestricted.".into());
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
struct CopyStatus {
    id: u64,
    operation: &'static str,
    state: String,
    done_files: u64,
    total_files: u64,
    done_bytes: u64,
    total_bytes: u64,
    current: Option<String>,
    skipped: u64,
    failed: usize,
    errors: Vec<String>,
    pending_conflict: Option<PendingConflict>,
    #[serde(skip_serializing)]
    conflict_sender: Option<mpsc::Sender<ConflictAction>>,
    #[serde(skip_serializing)]
    last_conflict_id: u64,
}

#[derive(Clone, Debug, Serialize)]
struct PendingConflict {
    id: u64,
    source: String,
    target: String,
    source_size: Option<u64>,
    target_size: Option<u64>,
    source_is_dir: bool,
    target_is_dir: bool,
}

impl PendingConflict {
    fn new(id: u64, source: &Path, target: &Path, state: &AppState) -> Self {
        let source_metadata = fs::symlink_metadata(source).ok();
        let target_metadata = fs::symlink_metadata(target).ok();
        Self {
            id,
            source: display_user_path(source, state),
            target: display_user_path(target, state),
            source_size: source_metadata.as_ref().filter(|m| m.is_file()).map(fs::Metadata::len),
            target_size: target_metadata.as_ref().filter(|m| m.is_file()).map(fs::Metadata::len),
            source_is_dir: source_metadata.as_ref().is_some_and(fs::Metadata::is_dir),
            target_is_dir: target_metadata.as_ref().is_some_and(fs::Metadata::is_dir),
        }
    }
}

fn display_user_path(path: &Path, state: &AppState) -> String {
    if let Ok(relative) = path.strip_prefix(&state.data) {
        return format!("DATA:/{}", relative.display());
    }
    if let Ok(relative) = path.strip_prefix(&state.models) {
        return format!("MODELS:/{}", relative.display());
    }
    path.file_name().map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "(soubor)".to_owned())
}

impl CopyStatus {
    fn new(id: u64, operation: &'static str) -> Self {
        Self {
            id,
            operation,
            state: "preparing".to_owned(),
            done_files: 0,
            total_files: 0,
            done_bytes: 0,
            total_bytes: 0,
            current: None,
            skipped: 0,
            failed: 0,
            errors: Vec::new(),
            pending_conflict: None,
            conflict_sender: None,
            last_conflict_id: 0,
        }
    }
}

fn canonical_root(key: &str, default: &str) -> Result<PathBuf, String> {
    let configured = std::env::var(key).unwrap_or_else(|_| default.to_owned());
    let path = fs::canonicalize(&configured)
        .map_err(|e| format!("{key} ({configured}): {e}"))?;
    if !path.is_dir() {
        return Err(format!("{key} ({configured}) není adresář"));
    }
    Ok(path)
}

#[derive(Deserialize)]
struct ListQuery {
    root: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    hidden: bool,
}

#[derive(Serialize)]
struct ListEntry {
    name: String,
    kind: &'static str,
    directory: bool,
    size: u64,
    modified_unix: Option<u64>,
    permissions: String,
}

#[derive(Serialize)]
struct ListResponse {
    root: String,
    path: String,
    parent: Option<String>,
    entries: Vec<ListEntry>,
    warnings: Vec<String>,
    storage: Option<FilesystemSpace>,
    // Filled only after AI Platform supplies measured project-quota usage.
    // None does NOT mean the user has no quota.
    quota: Option<QuotaUsage>,
}

#[derive(Serialize)]
struct FilesystemSpace {
    total_bytes: u64,
    used_bytes: u64,
    free_bytes: u64,
    available_bytes: u64,
}

#[derive(Serialize)]
struct QuotaUsage {
    used_bytes: u64,
    soft_bytes: Option<u64>,
    hard_bytes: Option<u64>,
}

// Evaluate the filesystem of the CURRENT directory, not the configured root.
// A subdirectory can be a mountpoint on a completely different filesystem.
// statvfs reports WHOLE-filesystem usage, not XFS project quota usage.
fn filesystem_space(directory: &Path) -> io::Result<FilesystemSpace> {
    let name = CString::new(directory.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL v cestě"))?;
    let mut result = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: name is a NUL-terminated pathname; result points to valid writable memory.
    if unsafe { libc::statvfs(name.as_ptr(), result.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful statvfs initialized result.
    let stat = unsafe { result.assume_init() };
    let unit = if stat.f_frsize > 0 { stat.f_frsize as u64 } else { stat.f_bsize as u64 };
    let total_bytes = (stat.f_blocks as u64).saturating_mul(unit);
    let free_bytes = (stat.f_bfree as u64).saturating_mul(unit);
    let available_bytes = (stat.f_bavail as u64).saturating_mul(unit);
    Ok(FilesystemSpace {
        total_bytes,
        used_bytes: total_bytes.saturating_sub(free_bytes),
        free_bytes,
        available_bytes,
    })
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: &str) -> Self {
        Self { status: StatusCode::BAD_REQUEST, message: message.to_owned() }
    }
    fn forbidden(message: &str) -> Self {
        Self { status: StatusCode::FORBIDDEN, message: message.to_owned() }
    }
    fn not_found(message: &str) -> Self {
        Self { status: StatusCode::NOT_FOUND, message: message.to_owned() }
    }
    fn conflict(message: &str) -> Self {
        Self { status: StatusCode::CONFLICT, message: message.to_owned() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(json!({"error": self.message}))).into_response();
        response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}

/// Resolve a user-supplied relative directory inside one configured root.
/// Canonicalization blocks symlinks that point outside that root.
/// This is a read-only *prototype*, not an openat2-based write sandbox.
fn resolve_directory(root: &Path, input: &str) -> Result<(PathBuf, String), ApiError> {
    let relative = Path::new(input);
    for component in relative.components() {
        if !matches!(component, Component::Normal(_) | Component::CurDir) {
            return Err(ApiError::forbidden("Cesta nesmí opustit zvolený strom."));
        }
    }
    let absolute = fs::canonicalize(root.join(relative)).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ApiError::not_found("Adresář neexistuje."),
        std::io::ErrorKind::PermissionDenied => ApiError::forbidden("Přístup odepřen."),
        _ => ApiError::bad_request("Adresář nelze otevřít."),
    })?;
    let safe_relative = absolute.strip_prefix(root)
        .map_err(|_| ApiError::forbidden("Cesta míří mimo povolený strom."))?;
    if !absolute.is_dir() {
        return Err(ApiError::bad_request("Cesta neoznačuje adresář."));
    }
    let display_path = safe_relative.to_str()
        .ok_or_else(|| ApiError::bad_request("Cesta nemá platné kódování UTF-8."))?
        .to_owned();
    Ok((absolute, display_path))
}

async fn list_directory(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let root = state.root(&query.root)?;
    let (directory, relative) = resolve_directory(root, &query.path)?;

    // The shared core performs metadata/ACL-aware listing.
    // Reading a directory is normally cheap but may be slow on NAS mounts;
    // use a blocking worker so that other HTTP requests remain responsive.
    let hidden = query.hidden;
    let (listing, storage) = tokio::task::spawn_blocking(move || {
        let listing = model::read_directory(&directory, hidden);
        let storage = filesystem_space(&directory).ok();
        (listing, storage)
    })
        .await
        .map_err(|_| ApiError::bad_request("Načítání adresáře selhalo."))?;
    let listing = listing.map_err(|e| match e.kind() {
            std::io::ErrorKind::PermissionDenied => ApiError::forbidden("Přístup odepřen."),
            std::io::ErrorKind::NotFound => ApiError::not_found("Adresář již neexistuje."),
            _ => ApiError::bad_request("Adresář nelze načíst."),
        })?;

    let mut entries = Vec::with_capacity(listing.entries.len());
    let mut warnings: Vec<String> = listing.warnings.into_iter().map(|e| e.error).collect();
    for entry in listing.entries {
        let Some(name) = entry.name.to_str() else {
            warnings.push("Položka s názvem mimo UTF-8 zatím není zobrazena.".to_owned());
            continue;
        };
        // Server-owned resumable transfer checkpoints are not user files.
        if name.starts_with(".salamander-resume-") { continue; }
        let kind = match entry.kind {
            EntryKind::Directory => "directory",
            EntryKind::File => "file",
            EntryKind::Symlink => "symlink",
            EntryKind::Other => "other",
            EntryKind::Parent => continue,
        };
        let modified_unix = entry.modified
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
        entries.push(ListEntry {
            name: name.to_owned(),
            kind,
            directory: entry.is_dir_like(),
            size: entry.size,
            modified_unix,
            permissions: entry.mode_text(),
        });
    }
    entries.sort_unstable_by(|a, b| {
        b.directory.cmp(&a.directory)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });

    let parent = if relative.is_empty() {
        None
    } else {
        Some(Path::new(&relative).parent().unwrap_or(Path::new(""))
            .to_string_lossy().to_string())
    };

    let result = ListResponse {
        root: query.root,
        path: relative,
        parent,
        entries,
        warnings,
        storage,
        quota: None,
    };
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(result)))
}

/// Default mode: writes disabled; explicit writes are Landlock-confined
/// to the two configured roots anywhere on the filesystem.
/// --unrestricted disables the kernel sandbox, but
/// does NOT make race-prone common-core operations production-safe.
/// The browser interface must never be exposed to a public network.
#[derive(Deserialize)]
struct CopyRequest {
    source_root: String,
    source_path: String,
    names: Vec<String>,
    dest_root: String,
    dest_path: String,
}

#[derive(Deserialize)]
struct CopyStatusQuery {
    id: u64,
}

#[derive(Deserialize)]
struct ConflictResolution {
    id: u64,
    conflict_id: u64,
    action: String,
    new_name: Option<String>,
}

#[derive(Deserialize)]
struct DirectoryRequest {
    root: String,
    path: String,
    name: String,
}

#[derive(Deserialize)]
struct RenameRequest {
    root: String,
    path: String,
    name: String,
    new_name: String,
}

#[derive(Deserialize)]
struct DeleteRequest {
    root: String,
    path: String,
    names: Vec<String>,
    confirm: bool,
}

#[derive(Deserialize)]
struct PackRequest {
    source_root: String,
    source_path: String,
    names: Vec<String>,
    dest_root: String,
    dest_path: String,
    archive_name: String,
}

#[derive(Deserialize)]
struct ExtractArchiveRequest {
    source_root: String,
    source_path: String,
    name: String,
    dest_root: String,
    dest_path: String,
}

fn valid_entry_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(".salamander-resume-")
        && name.len() <= 255
        && !name.contains('/')
        && !name.contains('\0')
        && matches!(Path::new(name).components().next(), Some(Component::Normal(_)))
        && Path::new(name).components().count() == 1
}

fn validate_selected_paths(
    state: &AppState, root_name: &str, relative: &str, names: &[String]
) -> Result<(Vec<PathBuf>, PathBuf), ApiError> {
    if names.is_empty() || names.len() > 128 {
        return Err(ApiError::bad_request("Označte 1 až 128 položek."));
    }
    let (dir, _) = resolve_directory(state.root(root_name)?, relative)?;
    let mut seen = HashSet::new();
    let mut paths = Vec::with_capacity(names.len());
    for name in names {
        if !valid_entry_name(name) || !seen.insert(name.as_str()) {
            return Err(ApiError::bad_request("Neplatný nebo duplicitní název položky."));
        }
        let path = dir.join(name);
        let metadata = fs::symlink_metadata(&path).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => ApiError::not_found("Zdrojová položka již neexistuje."),
            io::ErrorKind::PermissionDenied => ApiError::forbidden("Nelze číst zdroj."),
            _ => ApiError::bad_request("Zdrojovou položku nelze otevřít."),
        })?;
        // Reject symlinks and special objects at the selected top level.
        // The common core still needs descriptor-based hardening for nested paths.
        if !(metadata.is_file() || metadata.is_dir()) {
            return Err(ApiError::bad_request("Lze vybrat pouze běžný soubor nebo adresář, nikoliv symbolický odkaz."));
        }
        paths.push(path);
    }
    Ok((paths, dir))
}

fn validate_copy_paths(state: &AppState, req: &CopyRequest) -> Result<(Vec<PathBuf>, PathBuf), ApiError> {
    let dest_root = state.root(&req.dest_root)?;
    let (sources, source_dir) = validate_selected_paths(state, &req.source_root, &req.source_path, &req.names)?;
    let (dest_dir, _) = resolve_directory(dest_root, &req.dest_path)?;
    if source_dir == dest_dir {
        return Err(ApiError::bad_request("Zdroj a cíl jsou stejný adresář."));
    }
    for source in &sources {
        let metadata = fs::symlink_metadata(source)
            .map_err(|_| ApiError::not_found("Zdroj již neexistuje."))?;
        if metadata.is_dir() && dest_dir.starts_with(source) {
            return Err(ApiError::bad_request("Adresář nelze kopírovat do vlastního podadresáře."));
        }
        // A directory may be changed by other processes later; this is only a
        // preflight check, not a production-grade path-security guarantee.
    }
    Ok((sources, dest_dir))
}

async fn copy_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CopyRequest>,
) -> Result<impl IntoResponse, ApiError> {
    transfer_start(state, headers, req, OpKind::Copy, "copy").await
}

async fn move_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CopyRequest>,
) -> Result<impl IntoResponse, ApiError> {
    transfer_start(state, headers, req, OpKind::Move, "move").await
}

// Not authentication: the custom header only blocks ordinary HTML form POSTs.
fn require_write_header(headers: &HeaderMap, expected: &str) -> Result<(), ApiError> {
    if headers.get("x-salamander-request").and_then(|h| h.to_str().ok()) != Some(expected) {
        return Err(ApiError::forbidden("Neplatný požadavek."));
    }
    Ok(())
}

async fn transfer_start(
    state: AppState, headers: HeaderMap, req: CopyRequest,
    kind: OpKind, operation: &'static str,
) -> Result<impl IntoResponse, ApiError> {
    require_write_header(&headers, operation)?;
    let permit = state.write_permit()?;
    let (sources, dest_dir) = tokio::task::spawn_blocking({
        let state = state.clone();
        move || validate_copy_paths(&state, &req)
    })
    .await
    .map_err(|_| ApiError::bad_request("Kontrola souborů selhala."))??;

    let id = state.next_job_id.fetch_add(1, Ordering::Relaxed);
    let (events_tx, events_rx) = async_channel::bounded(64);
    let (replies_tx, replies_rx) = mpsc::channel();
    {
        let mut current = state.job.lock()
            .map_err(|_| ApiError::bad_request("Stav kopírování není dostupný."))?;
        let mut job = CopyStatus::new(id, operation);
        job.conflict_sender = Some(replies_tx);
        *current = Some(job);
    }

    let cancel = Arc::new(AtomicBool::new(false));
    let worker = ops::start(Op { kind, sources, dest_dir }, events_tx, replies_rx, cancel);
    let shared_job = Arc::clone(&state.job);
    tokio::spawn(async move {
        while let Ok(event) = events_rx.recv().await {
            let finished = matches!(&event, OpEvent::Finished(_));
            if let Ok(mut slot) = shared_job.lock() {
                if let Some(job) = slot.as_mut().filter(|job| job.id == id) {
                    match event {
                        OpEvent::Preparing { current } => {
                            job.state = "preparing".to_owned();
                            job.current = Some(current.display().to_string());
                        }
                        OpEvent::Progress(p) => {
                            job.state = "running".to_owned();
                            job.done_files = p.done_files;
                            job.total_files = p.total_files;
                            job.done_bytes = p.done_bytes;
                            job.total_bytes = p.total_bytes;
                            job.current = Some(p.current.display().to_string());
                        }
                        OpEvent::Conflict(details) => {
                            job.last_conflict_id += 1;
                            let seq = job.last_conflict_id;
                            job.pending_conflict = Some(PendingConflict::new(
                                seq, &details.source, &details.target, &state,
                            ));
                            job.state = "waiting_conflict".to_owned();
                            // A closed browser must not leave a worker holding
                            // the write lock forever. Unanswered conflicts
                            // are cancelled automatically after ten minutes.
                            let watchdog_jobs = Arc::clone(&shared_job);
                            tokio::spawn(async move {
                                tokio::time::sleep(std::time::Duration::from_secs(600)).await;
                                if let Ok(mut slot) = watchdog_jobs.lock() {
                                    if let Some(waiting) = slot.as_mut().filter(|job| job.id == id) {
                                        if waiting.state == "waiting_conflict"
                                            && waiting.pending_conflict.as_ref().is_some_and(|c| c.id == seq)
                                        {
                                            if let Some(sender) = waiting.conflict_sender.as_ref() {
                                                let _ = sender.send(ConflictAction::Abort);
                                            }
                                            waiting.pending_conflict = None;
                                            waiting.state = "resolving".to_owned();
                                        }
                                    }
                                }
                            });
                        }
                        OpEvent::Finished(result) => {
                            job.state = if result.outcome == Outcome::Cancelled {
                                "cancelled".to_owned()
                            } else {
                                "finished".to_owned()
                            };
                            job.failed = result.errors.len();
                            job.skipped = result.skipped;
                            job.errors = result.errors.into_iter().take(12).map(|error| {
                                format!("{}: {}", error.source.display(), error.error)
                            }).collect();
                            job.current = None;
                            job.pending_conflict = None;
                            job.conflict_sender = None;
                        }
                    }
                }
            }
            if finished { break; }
        }
        let joined = tokio::task::spawn_blocking(move || worker.join()).await;
        if !matches!(joined, Ok(Ok(()))) {
            if let Ok(mut slot) = shared_job.lock() {
                if let Some(job) = slot.as_mut().filter(|job| job.id == id) {
                    job.state = "failed".to_owned();
                    job.pending_conflict = None;
                    job.conflict_sender = None;
                    job.failed += 1;
                    job.errors.push("Pracovní vlákno se neočekávaně ukončilo.".to_owned());
                }
            }
        }
        drop(permit);
    });

    Ok((StatusCode::ACCEPTED, [(header::CACHE_CONTROL, "no-store")], Json(json!({"id": id}))))
}

async fn copy_status(
    State(state): State<AppState>,
    Query(query): Query<CopyStatusQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let slot = state.job.lock()
        .map_err(|_| ApiError::bad_request("Stav kopírování není dostupný."))?;
    let job = slot.as_ref().filter(|job| job.id == query.id)
        .ok_or_else(|| ApiError::not_found("Operace již není dostupná."))?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(job.clone())))
}

/// Answer the currently pending F5/F6 collision. The job ID and conflict ID
/// prevent stale browser tabs from replying to a later conflict or operation.
async fn copy_resolve(
    State(state): State<AppState>, headers: HeaderMap,
    Json(req): Json<ConflictResolution>,
) -> Result<impl IntoResponse, ApiError> {
    require_write_header(&headers, "resolve")?;
    let decision = match req.action.as_str() {
        "overwrite" => ConflictAction::Overwrite,
        "skip" => ConflictAction::Skip,
        "overwrite_all" => ConflictAction::OverwriteAll,
        "skip_all" => ConflictAction::SkipAll,
        "abort" => ConflictAction::Abort,
        "rename" => {
            let name = req.new_name.as_deref()
                .ok_or_else(|| ApiError::bad_request("Chybí nový název."))?;
            if !valid_entry_name(name) {
                return Err(ApiError::bad_request("Neplatný nový název."));
            }
            ConflictAction::Rename(name.to_owned())
        }
        _ => return Err(ApiError::bad_request("Neznámé rozhodnutí o konfliktu.")),
    };
    let mut slot = state.job.lock()
        .map_err(|_| ApiError::bad_request("Stav operace není dostupný."))?;
    let job = slot.as_mut().filter(|job| job.id == req.id)
        .ok_or_else(|| ApiError::not_found("Operace již není dostupná."))?;
    if job.state != "waiting_conflict"
        || job.pending_conflict.as_ref().is_none_or(|c| c.id != req.conflict_id)
    {
        return Err(ApiError::conflict("Tento konflikt již není aktuální."));
    }
    job.conflict_sender.as_ref()
        .ok_or_else(|| ApiError::conflict("Operace už skončila."))?
        .send(decision)
        .map_err(|_| ApiError::conflict("Operace už skončila."))?;
    job.pending_conflict = None;
    job.state = "resolving".to_owned();
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(json!({"ok":true}))))
}

async fn create_directory(
    State(state): State<AppState>, headers: HeaderMap,
    Json(req): Json<DirectoryRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_write_header(&headers, "mkdir")?;
    let _permit = state.write_permit()?;
    tokio::task::spawn_blocking(move || {
        if !valid_entry_name(&req.name) {
            return Err(ApiError::bad_request("Neplatný název adresáře."));
        }
        let (dir, _) = resolve_directory(state.root(&req.root)?, &req.path)?;
        fs::create_dir(dir.join(&req.name))
            .map_err(|e| if e.kind() == io::ErrorKind::AlreadyExists {
                ApiError::conflict("Položka s tímto názvem již existuje.")
            } else { ApiError::bad_request(&format!("Vytvoření adresáře selhalo: {e}")) })?;
        Ok::<_, ApiError>(())
    }).await.map_err(|_| ApiError::bad_request("Vytvoření adresáře selhalo."))??;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(json!({"ok": true}))))
}

async fn rename_entry(
    State(state): State<AppState>, headers: HeaderMap,
    Json(req): Json<RenameRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_write_header(&headers, "rename")?;
    let _permit = state.write_permit()?;
    tokio::task::spawn_blocking(move || {
        if !valid_entry_name(&req.name) || !valid_entry_name(&req.new_name) {
            return Err(ApiError::bad_request("Neplatný název položky."));
        }
        if req.name == req.new_name {
            return Err(ApiError::bad_request("Název se nezměnil."));
        }
        let (paths, dir) = validate_selected_paths(&state, &req.root, &req.path, &[req.name])?;
        let target = dir.join(&req.new_name);
        // Do not overwrite even a broken symlink.
        if fs::symlink_metadata(&target).is_ok() {
            return Err(ApiError::conflict("Cílová položka již existuje."));
        }
        fs::rename(&paths[0], &target)
            .map_err(|e| ApiError::bad_request(&format!("Přejmenování selhalo: {e}")))?;
        Ok::<_, ApiError>(())
    }).await.map_err(|_| ApiError::bad_request("Přejmenování selhalo."))??;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(json!({"ok": true}))))
}

async fn delete_entries(
    State(state): State<AppState>, headers: HeaderMap,
    Json(req): Json<DeleteRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_write_header(&headers, "delete")?;
    if !req.confirm {
        return Err(ApiError::bad_request("Mazání vyžaduje výslovné potvrzení."));
    }
    let _permit = state.write_permit()?;
    let (count, errors) = tokio::task::spawn_blocking(move || {
        let (paths, _) = validate_selected_paths(&state, &req.root, &req.path, &req.names)?;
        let count = paths.len();
        Ok::<_, ApiError>((count, ops::delete_permanently(paths)))
    }).await.map_err(|_| ApiError::bad_request("Mazání selhalo."))??;
    if !errors.is_empty() {
        return Err(ApiError::bad_request(&format!(
            "Mazání částečně selhalo ({} z {} položek): {}",
            errors.len(), count, errors[0].error,
        )));
    }
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(json!({"ok": true, "deleted": count}))))
}

fn new_background_job(state: &AppState, operation: &'static str) -> Result<u64, ApiError> {
    let id = state.next_job_id.fetch_add(1, Ordering::Relaxed);
    let mut slot = state.job.lock()
        .map_err(|_| ApiError::bad_request("Stav operací není dostupný."))?;
    *slot = Some(CopyStatus::new(id, operation));
    Ok(id)
}

fn zip_progress(shared: &Arc<Mutex<Option<CopyStatus>>>, id: u64, path: &Path) {
    if let Ok(mut slot) = shared.lock() {
        if let Some(job) = slot.as_mut().filter(|job| job.id == id) {
            job.state = "running".to_owned();
            job.done_files += 1;
            job.current = Some(path.display().to_string());
        }
    }
}

fn zip_finished(shared: &Arc<Mutex<Option<CopyStatus>>>, id: u64, result: archive::JobResult) {
    if let Ok(mut slot) = shared.lock() {
        if let Some(job) = slot.as_mut().filter(|job| job.id == id) {
            job.state = if result.cancelled { "cancelled".to_owned() }
                else if !result.errors.is_empty() { "failed".to_owned() }
                else { "finished".to_owned() };
            job.failed = result.errors.len();
            job.errors = result.errors.into_iter().take(12).map(|e| {
                format!("{}: {}", e.source.display(), e.error)
            }).collect();
            job.current = None;
        }
    }
}

fn zip_worker_failed(shared: &Arc<Mutex<Option<CopyStatus>>>, id: u64) {
    if let Ok(mut slot) = shared.lock() {
        if let Some(job) = slot.as_mut().filter(|job| job.id == id) {
            job.state = "failed".to_owned();
            job.failed += 1;
            job.errors.push("Pracovní vlákno archivu selhalo.".to_owned());
        }
    }
}

/// Create a ZIP in the destination panel from selected items in the source panel.
/// The ZIP is created by the existing libarchive-based salamander-core module.
async fn pack_zip(
    State(state): State<AppState>, headers: HeaderMap,
    Json(req): Json<PackRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_write_header(&headers, "zip-create")?;
    let permit = state.write_permit()?;
    let (sources, target) = tokio::task::spawn_blocking({
        let state = state.clone();
        move || {
            if !valid_entry_name(&req.archive_name)
                || !req.archive_name.to_ascii_lowercase().ends_with(".zip") {
                return Err(ApiError::bad_request("Archiv musí mít platný název končící .zip."));
            }
            let (sources, _) = validate_selected_paths(&state, &req.source_root, &req.source_path, &req.names)?;
            let (dest_dir, _) = resolve_directory(state.root(&req.dest_root)?, &req.dest_path)?;
            for path in &sources {
                if fs::symlink_metadata(path).is_ok_and(|m| m.is_dir()) && dest_dir.starts_with(path) {
                    return Err(ApiError::bad_request("Nelze vytvořit ZIP uvnitř archivovaného adresáře."));
                }
            }
            let target = dest_dir.join(&req.archive_name);
            if fs::symlink_metadata(&target).is_ok() {
                return Err(ApiError::conflict("ZIP s tímto názvem už existuje; nepřepisujeme jej."));
            }
            Ok::<_, ApiError>((sources, target))
        }
    }).await.map_err(|_| ApiError::bad_request("Kontrola ZIP selhala."))??;
    let id = new_background_job(&state, "zip-create")?;
    let shared = Arc::clone(&state.job);
    tokio::spawn(async move {
        let progress_shared = Arc::clone(&shared);
        let finished = tokio::task::spawn_blocking(move || {
            let cancel = AtomicBool::new(false);
            archive::create(&target, PackFormat::Zip, &sources, &cancel,
                &mut |path| zip_progress(&progress_shared, id, path))
        }).await;
        match finished {
            Ok(result) => zip_finished(&shared, id, result),
            Err(_) => zip_worker_failed(&shared, id),
        }
        drop(permit);
    });
    Ok((StatusCode::ACCEPTED, [(header::CACHE_CONTROL, "no-store")], Json(json!({"id": id}))))
}

/// Limits apply to both preflight (declared data) and streaming extraction
/// (actual decompressed data). The tests deliberately stay under /tmp.
fn zip_limits() -> archive::ExtractLimits {
    archive::ExtractLimits {
        max_entries: 20_000,
        max_bytes: 20 * 1024 * 1024 * 1024,
        max_depth: 64,
        regular_files_only: true,
        require_zip: true,
    }
}

fn preflight_zip(source: &Path) -> Result<(), ApiError> {
    archive::validate_zip(source, zip_limits())
        .map_err(|e| ApiError::bad_request(&format!("Unsafe or unsupported ZIP: {e}")))
}

async fn extract_zip(
    State(state): State<AppState>, headers: HeaderMap,
    Json(req): Json<ExtractArchiveRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_write_header(&headers, "zip-extract")?;
    let permit = state.write_permit()?;
    let (source, dest) = tokio::task::spawn_blocking({
        let state = state.clone();
        move || {
            if !valid_entry_name(&req.name) || !req.name.to_ascii_lowercase().ends_with(".zip") {
                return Err(ApiError::bad_request("Vyberte soubor .zip."));
            }
            let (paths, _) = validate_selected_paths(&state, &req.source_root, &req.source_path, &[req.name])?;
            if !fs::symlink_metadata(&paths[0]).is_ok_and(|m| m.is_file()) {
                return Err(ApiError::bad_request("Zdroj není soubor ZIP."));
            }
            let (dest_dir, _) = resolve_directory(state.root(&req.dest_root)?, &req.dest_path)?;
            preflight_zip(&paths[0])?;
            Ok::<_, ApiError>((paths[0].clone(), dest_dir))
        }
    }).await.map_err(|_| ApiError::bad_request("Kontrola ZIP selhala."))??;
    let id = new_background_job(&state, "zip-extract")?;
    let shared = Arc::clone(&state.job);
    tokio::spawn(async move {
        let progress_shared = Arc::clone(&shared);
        let finished = tokio::task::spawn_blocking(move || {
            let cancel = AtomicBool::new(false);
            let empty: [PathBuf; 0] = [];
            archive::extract_limited(&ExtractRequest {
                archive: &source,
                items: &empty,
                base: Path::new(""),
                dest: &dest,
                overwrite: false,
            }, &cancel, &mut |path| zip_progress(&progress_shared, id, path),
            Some(zip_limits()))
        }).await;
        match finished {
            Ok(result) => zip_finished(&shared, id, result),
            Err(_) => zip_worker_failed(&shared, id),
        }
        drop(permit);
    });
    Ok((StatusCode::ACCEPTED, [(header::CACHE_CONTROL, "no-store")], Json(json!({"id": id}))))
}

async fn roots(State(state): State<AppState>) -> impl IntoResponse {
    let labels = if state.unrestricted {
        [format!("ROOT A ({})", state.data.display()),
         format!("ROOT B ({})", state.models.display())]
    } else {
        ["DATA".to_owned(), "MODELS".to_owned()]
    };
    ([(header::CACHE_CONTROL, "no-store")], Json(json!({
        "roots": [
            {"id": "data", "label": labels[0]},
            {"id": "models", "label": labels[1]}
        ]
    })))
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    let mode = if state.unrestricted { "unrestricted" } else if state.writes_enabled { "confined-write" } else { "read-only" };
    ([(header::CACHE_CONTROL, "no-store")], Json(json!({"ok": true, "mode": mode, "writes_enabled": state.writes_enabled, "unrestricted": state.unrestricted})))
}

async fn index() -> impl IntoResponse {
    (
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::CONTENT_SECURITY_POLICY,
             "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
        ],
        Html(include_str!("../static/index.html")),
    )
}

async fn css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8"),
      (header::CACHE_CONTROL, "no-store")],
     include_str!("../static/style.css"))
}

async fn js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
      (header::CACHE_CONTROL, "no-store")],
     include_str!("../static/app.js"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut unrestricted = false;
    let mut verify = false;
    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--unrestricted" => unrestricted = true,
            "--verify-sandbox" => verify = true,
            "--help" | "-h" => {
                println!("SalamanderRS Web\n\n  --unrestricted    Local administrator mode: no Landlock; requires SALAMANDER_ADMIN_PASSWORD\n  --verify-sandbox  Verify Landlock with the configured DATA/MODELS roots and exit\n\nDefault: read-only. Set SALAMANDER_ENABLE_WRITES=1 to permit writes within configured roots (Landlock required).\nConfined writable mode must run under a non-root UID/GID.\nAlways binds to 127.0.0.1. This prototype has no HTTP authentication in confined mode.");
                return Ok(());
            }
            _ => return Err(format!("Neznámý přepínač: {argument}").into()),
        }
    }
    if unrestricted && verify {
        return Err("--verify-sandbox nelze kombinovat s --unrestricted".into());
    }
    let state = AppState::from_env(unrestricted)?;

    // Prepare outside probes BEFORE Landlock seals the process, so that
    // --verify-sandbox also works when either root is located under /tmp.
    let probes = if verify {
        if !state.writes_enabled {
            return Err("--verify-sandbox requires SALAMANDER_ENABLE_WRITES=1".into());
        }
        Some(security::prepare_verification(&state.data, &state.models)?)
    } else {
        None
    };

    // Lock down the current thread before creating Tokio worker threads.
    // In explicit administrator mode Landlock is intentionally disabled.
    if state.writes_enabled && !state.unrestricted {
        security::enter(&state.data_fd, &state.models_fd)?;
    }

    if let Some(probes) = probes {
        security::verify(&state.data_fd, &state.models_fd, &state.data, &state.models, &probes)?;
        println!("Landlock verification succeeded: outside reads and writes denied.");
        return Ok(());
    }

    let port = std::env::var("SALAMANDER_PORT")
        .unwrap_or_else(|_| "8090".to_owned())
        .parse::<u16>()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all().build()?;
    runtime.block_on(serve(state, port))
}

/// Allow only literal loopback HTTP hostnames in privileged mode.
/// Reverse proxies and DNS-rebound hostnames are intentionally rejected.
fn loopback_host(value: &str) -> bool {
    let mut parts = value.split(':');
    let name = parts.next();
    let port = parts.next();
    let extra = parts.next();
    matches!(name, Some("localhost" | "127.0.0.1"))
        && extra.is_none()
        && port.is_none_or(|p| p.parse::<u16>().is_ok_and(|n| n != 0))
}

/// HTTP Basic auth is mandatory when the filesystem sandbox is disabled.
/// The server is only available on loopback and never advertises CORS.
async fn local_admin_auth(
    State(state): State<AppState>, request: Request, next: Next,
) -> Response {
    if let Some(secret) = state.admin_password.as_ref() {
        let host = request.headers().get(header::HOST).and_then(|h| h.to_str().ok());
        if !host.is_some_and(loopback_host) {
            return (StatusCode::FORBIDDEN, "Neomezený režim vyžaduje Host: localhost nebo 127.0.0.1.").into_response();
        }
        // Keep browser requests same-origin even if a local intermediary is
        // accidentally configured. Origin may be absent in some same-origin
        // clients; the custom per-action header still protects write routes.
        if request.method() == Method::POST {
            if let Some(origin) = request.headers().get(header::ORIGIN).and_then(|h| h.to_str().ok()) {
                if origin != format!("http://{}", host.unwrap_or_default()) {
                    return (StatusCode::FORBIDDEN, "Požadavek pochází z jiné webové stránky.").into_response();
                }
            }
        }
        if !secret.accepts(request.headers().get(header::AUTHORIZATION)) {
            return (
                StatusCode::UNAUTHORIZED,
                [
                    (header::WWW_AUTHENTICATE, "Basic realm=\"SalamanderRS local administrator\", charset=\"UTF-8\""),
                    (header::CACHE_CONTROL, "no-store"),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                ],
                "Vyžadováno přihlášení správce."
            ).into_response();
        }
    }
    next.run(request).await
}

async fn serve(state: AppState, port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let app = Router::new()
        .route("/", get(index))
        .route("/static/style.css", get(css))
        .route("/static/app.js", get(js))
        .route("/api/health", get(health))
        .route("/api/roots", get(roots))
        .route("/api/list", get(list_directory))
        .route("/api/download", get(transfer_http::download_file))
        .route("/api/resumable/start", post(resumable::start))
        .route("/api/resumable/status", get(resumable::status))
        .route("/api/resumable/chunk", post(resumable::chunk))
        .route("/api/resumable/finish", post(resumable::finish))
        .route("/api/resumable/cancel", post(resumable::cancel))
        .route("/api/preview/info", get(preview::info))
        .route("/api/preview/image", get(preview::image))
        .route("/api/upload", post(transfer_http::upload_file))
        .route("/api/file/create", post(transfer_http::create_empty_file))
        .route("/api/copy", post(copy_start))
        .route("/api/move", post(move_start))
        .route("/api/copy/status", get(copy_status))
        .route("/api/copy/resolve", post(copy_resolve))
        .route("/api/mkdir", post(create_directory))
        .route("/api/rename", post(rename_entry))
        .route("/api/delete", post(delete_entries))
        .route("/api/zip/create", post(pack_zip))
        .route("/api/zip/extract", post(extract_zip))
        .route("/api/editor/open", get(transfer_http::open_text))
        .route("/api/editor/save", post(transfer_http::save_text)
            .layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024)))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(state.clone(), local_admin_auth));

    // Deliberately loopback-only; no auth is included in this prototype.
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let mode = if state.unrestricted { "NEOMEZENÝ REŽIM – bez Landlock, s HTTP Basic auth; pouze přes SSH tunel" }
        else if state.writes_enabled { "zápis omezený Landlockem na DATA/MODELS, bez HTTP autentizace" }
        else { "pouze čtení" };
    println!("SalamanderRS Web: http://{addr} ({mode})");
    if state.unrestricted {
        eprintln!("VAROVÁNÍ: --unrestricted běží s oprávněními procesu a vypíná kernelový sandbox. Nenastavuj veřejný listener/proxy! Pracuj přes SSH tunel.");
    }
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn privileged_mode_rejects_non_loopback_hosts() {
        assert!(loopback_host("127.0.0.1:8090"));
        assert!(loopback_host("localhost:8090"));
        assert!(!loopback_host("example.org:8090"));
        assert!(!loopback_host("localhost.evil.example:8090"));
        assert!(!loopback_host("localhost:abc"));
    }

    #[test]
    fn confined_roots_allow_arbitrary_subdirectories() {
        assert!(validate_confined_roots(
            Path::new("/AI/data/karel"), Path::new("/AI/models/karel")
        ).is_ok());
        assert!(validate_confined_roots(
            Path::new("/tmp/salamander-test/data"),
            Path::new("/tmp/salamander-test/models")
        ).is_ok());
        assert!(validate_confined_roots(
            Path::new("/data"), Path::new("/models")
        ).is_ok());
    }

    #[test]
    fn confined_mode_rejects_host_root() {
        assert!(validate_confined_roots(Path::new("/"), Path::new("/models")).is_err());
        assert!(validate_confined_roots(Path::new("/data"), Path::new("/")).is_err());
    }

    #[test]
    fn names_and_write_interlock() {
        assert!(valid_entry_name("model.safetensors"));
        assert!(valid_entry_name("Nová složka"));
        for invalid in ["", ".", "..", "../escape", "a/b", "/etc", "a\0b"] {
            assert!(!valid_entry_name(invalid), "unexpected valid name: {invalid:?}");
        }
        let tmp = tempfile::tempdir().unwrap();
        let state = AppState {
            data: tmp.path().to_path_buf(),
            models: tmp.path().to_path_buf(),
            data_fd: Arc::new(fs::File::open(tmp.path()).unwrap()),
            models_fd: Arc::new(fs::File::open(tmp.path()).unwrap()),
            max_upload_bytes: 1024 * 1024,
            writes_enabled: true,
            unrestricted: false,
            admin_password: None,
            job: Arc::new(Mutex::new(None)),
            next_job_id: Arc::new(AtomicU64::new(1)),
            write_busy: Arc::new(AtomicBool::new(false)),
        };
        let permit = state.write_permit().unwrap();
        assert!(state.write_permit().is_err());
        drop(permit);
        assert!(state.write_permit().is_ok());
    }

    #[tokio::test]
    async fn conflict_reply_rejects_stale_id_and_accepts_rename() {
        let tmp = tempfile::tempdir().unwrap();
        let state = AppState {
            data: tmp.path().to_path_buf(),
            models: tmp.path().to_path_buf(),
            data_fd: Arc::new(fs::File::open(tmp.path()).unwrap()),
            models_fd: Arc::new(fs::File::open(tmp.path()).unwrap()),
            max_upload_bytes: 1024,
            writes_enabled: true,
            unrestricted: false,
            admin_password: None,
            job: Arc::new(Mutex::new(None)),
            next_job_id: Arc::new(AtomicU64::new(1)),
            write_busy: Arc::new(AtomicBool::new(false)),
        };
        let (sender, receiver) = mpsc::channel();
        let mut job = CopyStatus::new(7, "copy");
        job.state = "waiting_conflict".to_owned();
        job.last_conflict_id = 3;
        job.pending_conflict = Some(PendingConflict {
            id: 3, source: "DATA:/test.txt".into(), target: "MODELS:/test.txt".into(),
            source_size: Some(3), target_size: Some(2),
            source_is_dir: false, target_is_dir: false,
        });
        job.conflict_sender = Some(sender);
        *state.job.lock().unwrap() = Some(job);
        let mut headers = HeaderMap::new();
        headers.insert("x-salamander-request", HeaderValue::from_static("resolve"));
        let err = copy_resolve(State(state.clone()), headers.clone(), Json(ConflictResolution {
            id: 7, conflict_id: 2, action: "skip".into(), new_name: None,
        })).await.err().expect("stale conflict ID must fail");
        assert_eq!(err.status, StatusCode::CONFLICT);
        let response = copy_resolve(State(state.clone()), headers.clone(), Json(ConflictResolution {
            id: 7, conflict_id: 3, action: "rename".into(),
            new_name: Some("test (kopie).txt".into()),
        })).await.unwrap().into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(receiver.try_recv().unwrap(), ConflictAction::Rename("test (kopie).txt".into()));
        let err = copy_resolve(State(state), headers, Json(ConflictResolution {
            id: 7, conflict_id: 3, action: "overwrite".into(), new_name: None,
        })).await.err().expect("already answered conflict must fail");
        assert_eq!(err.status, StatusCode::CONFLICT);
    }

    #[test]
    fn zip_archive_can_be_created_and_extracted_by_core() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let models = tmp.path().join("models");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&models).unwrap();
        fs::create_dir(data.join("folder")).unwrap();
        fs::write(data.join("folder/hello.txt"), b"hello zip").unwrap();
        let zip = models.join("test.zip");
        let cancel = AtomicBool::new(false);
        let result = archive::create(
            &zip, PackFormat::Zip, &[data.join("folder")], &cancel, &mut |_| {},
        );
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(zip.is_file());
        preflight_zip(&zip).unwrap();

        let unpacked = tmp.path().join("unpacked");
        fs::create_dir(&unpacked).unwrap();
        let result = archive::extract(&ExtractRequest {
            archive: &zip,
            items: &[],
            base: Path::new(""),
            dest: &unpacked,
            overwrite: false,
        }, &cancel, &mut |_| {});
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(fs::read(unpacked.join("folder/hello.txt")).unwrap(), b"hello zip");
        // An existing target is not overwritten by extraction.
        fs::write(unpacked.join("folder/hello.txt"), b"unchanged").unwrap();
        let result = archive::extract(&ExtractRequest {
            archive: &zip,
            items: &[],
            base: Path::new(""),
            dest: &unpacked,
            overwrite: false,
        }, &cancel, &mut |_| {});
        assert!(!result.errors.is_empty());
        assert_eq!(fs::read(unpacked.join("folder/hello.txt")).unwrap(), b"unchanged");
    }

    #[test]
    fn zip_extraction_checks_real_decompressed_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("large.txt");
        fs::write(&source, vec![b'X'; 4096]).unwrap();
        let zip = tmp.path().join("test.zip");
        let cancel = AtomicBool::new(false);
        let created = archive::create(
            &zip, PackFormat::Zip, &[source], &cancel, &mut |_| {},
        );
        assert!(created.errors.is_empty(), "{:?}", created.errors);
        preflight_zip(&zip).unwrap();

        let dest = tmp.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let request = ExtractRequest {
            archive: &zip, items: &[], base: Path::new(""),
            dest: &dest, overwrite: false,
        };
        let limit = archive::ExtractLimits {
            max_entries: 10, max_bytes: 1024, max_depth: 10,
            regular_files_only: true, require_zip: true,
        };
        let result = archive::extract_limited(&request, &cancel, &mut |_| {}, Some(limit));
        assert!(!result.errors.is_empty(), "Byte budget should reject a 4 KiB entry");
        assert!(!dest.join("large.txt").exists(), "Partial output should be removed");

        let result = archive::extract_limited(&request, &cancel, &mut |_| {}, Some(
            archive::ExtractLimits { max_bytes: 8192, ..limit }
        ));
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert_eq!(fs::read(dest.join("large.txt")).unwrap().len(), 4096);
    }

    #[test]
    fn copying_rejects_invalid_names_and_recursive_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let models = tmp.path().join("models");
        fs::create_dir(&data).unwrap();
        fs::create_dir(&models).unwrap();
        fs::create_dir(data.join("folder")).unwrap();
        fs::create_dir(data.join("folder/child")).unwrap();
        fs::write(data.join("test.txt"), b"hello").unwrap();
        let state = AppState {
            data: fs::canonicalize(&data).unwrap(),
            models: fs::canonicalize(&models).unwrap(),
            data_fd: Arc::new(fs::File::open(&data).unwrap()),
            models_fd: Arc::new(fs::File::open(&models).unwrap()),
            max_upload_bytes: 1024 * 1024,
            writes_enabled: true,
            unrestricted: false,
            admin_password: None,
            job: Arc::new(Mutex::new(None)),
            next_job_id: Arc::new(AtomicU64::new(1)),
            write_busy: Arc::new(AtomicBool::new(false)),
        };
        let mut req = CopyRequest {
            source_root: "data".to_owned(), source_path: "".to_owned(),
            names: vec!["test.txt".to_owned()],
            dest_root: "models".to_owned(), dest_path: "".to_owned(),
        };
        assert!(validate_copy_paths(&state, &req).is_ok());
        req.names = vec!["../test.txt".to_owned()];
        assert!(validate_copy_paths(&state, &req).is_err());
        req.names = vec!["test.txt".to_owned(), "test.txt".to_owned()];
        assert!(validate_copy_paths(&state, &req).is_err());
        req.names = vec!["folder".to_owned()];
        req.dest_root = "data".to_owned();
        req.dest_path = "folder/child".to_owned();
        assert!(validate_copy_paths(&state, &req).is_err());
        req.names = vec!["test.txt".to_owned()];
        req.dest_path.clear();
        assert!(validate_copy_paths(&state, &req).is_err());
    }

    #[test]
    fn capacity_from_current_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let space = filesystem_space(tmp.path()).unwrap();
        assert!(space.total_bytes > 0);
        assert!(space.used_bytes <= space.total_bytes);
        assert!(space.available_bytes <= space.free_bytes);
        assert!(space.free_bytes <= space.total_bytes);
    }

    #[test]
    fn relative_path_cannot_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        assert!(resolve_directory(&root, "../").is_err());
        assert!(resolve_directory(&root, "/etc").is_err());
        assert!(resolve_directory(&root, "sub/../../").is_err());
        assert_eq!(resolve_directory(&root, "").unwrap().1, "");
    }

    #[test]
    fn symlink_to_outside_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("data");
        let other = tmp.path().join("other");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&other).unwrap();
        symlink(&other, root.join("escape")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        assert!(resolve_directory(&root, "escape").is_err());
        fs::create_dir(root.join("inside")).unwrap();
        symlink(root.join("inside"), root.join("link")).unwrap();
        assert_eq!(resolve_directory(&root, "link").unwrap().1, "inside");
    }
}
