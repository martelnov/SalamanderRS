//! Read-only F3 preview for text and AI-generated image metadata. Source files
//! are never rewritten. The image itself is streamed to the browser; only
//! bounded metadata/text are decoded by the server.
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::AsRawFd;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine;
use flate2::read::ZlibDecoder;
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;

use super::{ApiError, AppState, valid_entry_name};
use super::transfer_http::{checked_openat2, io_error, open_relative_directory};

const TEXT_LIMIT: u64 = 2 * 1024 * 1024;
const MAX_IMAGE_BYTES: u64 = 256 * 1024 * 1024;
const META_TOTAL_LIMIT: usize = 8 * 1024 * 1024;
const META_ENTRY_LIMIT: usize = 2 * 1024 * 1024;
const META_COUNT_LIMIT: usize = 128;

#[derive(Deserialize)]
pub(super) struct PreviewQuery {
    root: String,
    #[serde(default)] path: String,
    name: String,
}
#[derive(Serialize, Clone, Debug)]
struct MetaField {
    source: String,
    key: String,
    text: Option<String>,
    /// Original bytes (including the PNG tEXt/iTXt wrapper), for lossless export.
    raw_base64: String,
}
#[derive(Serialize)]
struct PreviewInfo {
    kind: &'static str,
    size: u64,
    mime: Option<&'static str>,
    text: Option<String>,
    truncated: bool,
    metadata: Vec<MetaField>,
    warnings: Vec<String>,
}

fn open_preview_file(state: &AppState, q: PreviewQuery) -> Result<(File,u64), ApiError> {
    if !valid_entry_name(&q.name) { return Err(ApiError::bad_request("Neplatný název souboru.")); }
    let parent = open_relative_directory(state.root_handle(&q.root)?, &q.path)?;
    let name = CString::new(q.name).map_err(|_| ApiError::bad_request("Neplatný název."))?;
    let file = checked_openat2(parent.as_raw_fd(), &name,
        libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .map_err(|e| io_error("Otevření náhledu", e))?;
    let meta = file.metadata().map_err(|e| io_error("Kontrola souboru", e))?;
    if !meta.is_file() { return Err(ApiError::bad_request("Náhled je určen pouze pro běžné soubory.")); }
    Ok((file,meta.len()))
}
fn sniff(file: &mut File) -> Result<Option<&'static str>, ApiError> {
    let mut head = [0u8;16];
    let count = file.read(&mut head).map_err(|e| io_error("Čtení hlavičky obrázku", e))?;
    file.seek(SeekFrom::Start(0)).map_err(|e| io_error("Obnovení pozice souboru", e))?;
    let b = &head[..count];
    Ok(if b.starts_with(b"\x89PNG\r\n\x1a\n") { Some("image/png") }
        else if b.starts_with(&[0xff,0xd8,0xff]) { Some("image/jpeg") }
        else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") { Some("image/gif") }
        else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" { Some("image/webp") }
        else { None })
}
fn field(fields: &mut Vec<MetaField>, budget: &mut usize, source: &str, key: &str, text: Option<String>, raw: &[u8]) {
    if fields.len() >= META_COUNT_LIMIT || raw.len() > META_ENTRY_LIMIT || raw.len() > *budget { return; }
    *budget -= raw.len();
    fields.push(MetaField { source:source.into(), key:key.into(), text,
        raw_base64:base64::engine::general_purpose::STANDARD.encode(raw) });
}
fn bytes_text(bytes: &[u8]) -> Option<String> {
    if bytes.contains(&0) { return None; }
    String::from_utf8(bytes.to_vec()).ok()
}
fn inflate_limited(bytes: &[u8]) -> Result<String, &'static str> {
    let mut decoded = Vec::new();
    ZlibDecoder::new(bytes).take(META_ENTRY_LIMIT as u64 + 1)
        .read_to_end(&mut decoded).map_err(|_| "Poškozená komprimovaná metadata")?;
    if decoded.len() > META_ENTRY_LIMIT { return Err("Komprimovaná metadata překračují limit 2 MiB"); }
    bytes_text(&decoded).ok_or("Metadata nejsou platný UTF-8 text")
}
fn split_nul(bytes: &[u8]) -> Option<(&[u8], &[u8])> {
    let p = bytes.iter().position(|b| *b == 0)?;
    Some((&bytes[..p], &bytes[p+1..]))
}
fn png(file: &mut File, fields: &mut Vec<MetaField>, warnings: &mut Vec<String>, budget: &mut usize) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(8))?;
    for _ in 0..4096 {
        let mut header = [0u8;8];
        if let Err(err) = file.read_exact(&mut header) {
            if err.kind() == std::io::ErrorKind::UnexpectedEof { break; }
            return Err(err);
        }
        let size = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
        let kind = &header[4..8];
        if (kind == b"tEXt" || kind == b"zTXt" || kind == b"iTXt" || kind == b"eXIf") && size <= META_ENTRY_LIMIT && size <= *budget {
            let mut raw = vec![0u8;size];
            file.read_exact(&mut raw)?;
            if kind == b"eXIf" {
                field(fields,budget,"PNG eXIf","EXIF",None,&raw);
                exif(&raw,fields,budget,"PNG eXIf");
            } else if let Some((name, rest)) = split_nul(&raw) {
                let key = String::from_utf8_lossy(name).into_owned();
                let text = if kind == b"tEXt" {
                    bytes_text(rest).or_else(|| Some(rest.iter().map(|v| char::from(*v)).collect()))
                } else if kind == b"zTXt" && rest.first() == Some(&0) {
                    match inflate_limited(&rest[1..]) {
                        Ok(v) => Some(v), Err(err) => { warnings.push(err.into()); None }
                    }
                } else if kind == b"iTXt" && rest.len() >= 2 {
                    let compressed = rest[0] == 1 && rest[1] == 0;
                    let uncompressed = rest[0] == 0;
                    if let Some((_, remaining)) = split_nul(&rest[2..]) {
                        if let Some((_, value)) = split_nul(remaining) {
                            if compressed { match inflate_limited(value) {
                                Ok(v) => Some(v), Err(err) => { warnings.push(err.into()); None }
                            } } else if uncompressed { bytes_text(value) } else { None }
                        } else { None }
                    } else { None }
                } else { None };
                field(fields,budget,"PNG",&key,text,&raw);
            }
        } else {
            if (kind == b"tEXt" || kind == b"zTXt" || kind == b"iTXt" || kind == b"eXIf") && size > META_ENTRY_LIMIT {
                warnings.push("Jedna položka PNG metadat překročila limit 2 MiB a byla vynechána.".into());
            }
            file.seek(SeekFrom::Current(size as i64))?;
        }
        file.seek(SeekFrom::Current(4))?; // CRC (the image decoder validates it independently)
        if kind == b"IEND" { break; }
    }
    Ok(())
}
fn jpeg(file: &mut File, fields: &mut Vec<MetaField>, warnings: &mut Vec<String>, budget: &mut usize) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(2))?;
    for _ in 0..1024 {
        let mut start = [0u8;2];
        if file.read_exact(&mut start).is_err() { break; }
        if start[0] != 0xff { break; }
        let marker = start[1];
        if marker == 0xda || marker == 0xd9 { break; }
        if (0xd0..=0xd7).contains(&marker) || marker == 0x01 { continue; }
        let mut size_bytes = [0u8;2];
        file.read_exact(&mut size_bytes)?;
        let size = u16::from_be_bytes(size_bytes) as usize;
        if size < 2 { break; }
        let payload_size = size - 2;
        if matches!(marker, 0xe1 | 0xed | 0xfe) && payload_size <= META_ENTRY_LIMIT && payload_size <= *budget {
            let mut raw = vec![0u8;payload_size]; file.read_exact(&mut raw)?;
            if raw.starts_with(b"Exif\0\0") {
                field(fields,budget,"JPEG APP1","EXIF",None,&raw);
                exif(&raw[6..],fields,budget,"JPEG EXIF");
            } else if raw.starts_with(b"http://ns.adobe.com/xap/1.0/\0") {
                let offset = b"http://ns.adobe.com/xap/1.0/\0".len();
                field(fields,budget,"JPEG APP1","XMP",bytes_text(&raw[offset..]),&raw);
            } else if marker == 0xfe {
                field(fields,budget,"JPEG COM","Comment",bytes_text(&raw),&raw);
            } else if marker == 0xed {
                field(fields,budget,"JPEG APP13","Photoshop APP13",None,&raw);
            }
        } else { file.seek(SeekFrom::Current(payload_size as i64))?; }
    }
    if fields.len() >= META_COUNT_LIMIT { warnings.push("Byl dosažen limit počtu položek metadat.".into()); }
    Ok(())
}
fn webp(file: &mut File, fields: &mut Vec<MetaField>, budget: &mut usize) -> std::io::Result<()> {
    file.seek(SeekFrom::Start(12))?;
    for _ in 0..4096 {
        let mut header = [0u8;8];
        if file.read_exact(&mut header).is_err() { break; }
        let size = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        let kind = &header[..4];
        if (kind == b"EXIF" || kind == b"XMP ") && size <= META_ENTRY_LIMIT && size <= *budget {
            let mut raw = vec![0u8;size]; file.read_exact(&mut raw)?;
            if kind == b"XMP " { field(fields,budget,"WebP","XMP",bytes_text(&raw),&raw); }
            else {
                field(fields,budget,"WebP","EXIF",None,&raw);
                exif(raw.strip_prefix(b"Exif\0\0").unwrap_or(&raw),fields,budget,"WebP EXIF");
            }
        } else { file.seek(SeekFrom::Current(size as i64))?; }
        if size % 2 != 0 { file.seek(SeekFrom::Current(1))?; }
    }
    Ok(())
}
fn exif(buf: &[u8], fields: &mut Vec<MetaField>, budget: &mut usize, source: &str) {
    if buf.len() < 8 { return; }
    let little = if &buf[..2] == b"II" { true } else if &buf[..2] == b"MM" { false } else { return };
    let u16_at = |p:usize| -> Option<u16> {
        let x = buf.get(p..p+2)?; Some(if little {u16::from_le_bytes(x.try_into().ok()?)} else {u16::from_be_bytes(x.try_into().ok()?)})
    };
    let u32_at = |p:usize| -> Option<u32> {
        let x = buf.get(p..p+4)?; Some(if little {u32::from_le_bytes(x.try_into().ok()?)} else {u32::from_be_bytes(x.try_into().ok()?)})
    };
    if u16_at(2) != Some(42) { return; }
    let mut ifds = vec![u32_at(4).unwrap_or(0) as usize];
    for _ in 0..3 {
        let Some(offset) = ifds.pop() else { break };
        let Some(count) = u16_at(offset) else { continue };
        for n in 0..(count as usize).min(256) {
            let Some(p) = offset.checked_add(2).and_then(|v| v.checked_add(n*12)) else {break};
            let Some(tag) = u16_at(p) else {break};
            let Some(typ) = u16_at(p+2) else {break};
            let Some(count) = u32_at(p+4) else {break};
            if tag == 0x8769 && typ == 4 && count == 1 {
                ifds.push(u32_at(p+8).unwrap_or(0) as usize);
            }
            let key = match tag {
                0x010e => "ImageDescription", 0x0131 => "Software",
                0x9003 => "DateTimeOriginal", 0x9286 => "UserComment", _ => continue,
            };
            if !matches!(typ,2 | 7) || count == 0 || count as usize > META_ENTRY_LIMIT {continue;}
            let size = count as usize;
            let value = if size <= 4 { buf.get(p+8..p+8+size) }
                else { u32_at(p+8).and_then(|v| buf.get(v as usize..(v as usize).checked_add(size)?)) };
            let Some(value) = value else {continue};
            let value = if tag == 0x9286 && value.starts_with(b"ASCII\0\0\0") { &value[8..] } else {value};
            let value = value.strip_suffix(&[0]).unwrap_or(value);
            field(fields,budget,source,key,bytes_text(value),value);
        }
    }
}

fn preview_info(mut file: File, size: u64) -> Result<PreviewInfo, ApiError> {
    let mime = sniff(&mut file)?;
    if let Some(mime) = mime {
        let mut fields = Vec::new(); let mut warnings = Vec::new();
        let mut budget = META_TOTAL_LIMIT;
        match mime {
            "image/png" => png(&mut file,&mut fields,&mut warnings,&mut budget),
            "image/jpeg" => jpeg(&mut file,&mut fields,&mut warnings,&mut budget),
            "image/webp" => webp(&mut file,&mut fields,&mut budget),
            _ => Ok(()),
        }.map_err(|e| io_error("Čtení metadat obrázku", e))?;
        return Ok(PreviewInfo {kind:"image",size,mime:Some(mime),text:None,
            truncated:false, metadata:fields, warnings});
    }
    // Only bounded UTF-8 text, not arbitrary binary or embedded HTML, is shown.
    let mut data = Vec::new();
    Read::by_ref(&mut file).take(TEXT_LIMIT + 1).read_to_end(&mut data)
        .map_err(|e| io_error("Čtení textového náhledu", e))?;
    let truncated = data.len() as u64 > TEXT_LIMIT;
    if truncated {
        data.truncate(TEXT_LIMIT as usize);
        while !data.is_empty() && std::str::from_utf8(&data).is_err() { data.pop(); }
    }
    if data.contains(&0) { return Err(ApiError::bad_request("Tento typ souboru nemá podporovaný náhled.")); }
    let text = String::from_utf8(data).map_err(|_| ApiError::bad_request("Prohlížeč textu podporuje UTF-8."))?;
    Ok(PreviewInfo {kind:"text",size,mime:Some("text/plain"),text:Some(text),
        truncated,metadata:Vec::new(),warnings:Vec::new()})
}

pub(super) async fn info(
    State(state): State<AppState>, Query(q): Query<PreviewQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let result = tokio::task::spawn_blocking(move || {
        let (file,size) = open_preview_file(&state,q)?;
        preview_info(file,size)
    }).await.map_err(|_| ApiError::bad_request("Příprava náhledu selhala."))??;
    Ok(([(header::CACHE_CONTROL,"no-store"),(header::X_CONTENT_TYPE_OPTIONS,"nosniff")], Json(result)))
}

pub(super) async fn image(
    State(state): State<AppState>, Query(q): Query<PreviewQuery>,
) -> Result<Response, ApiError> {
    let (mut file,size) = tokio::task::spawn_blocking(move || open_preview_file(&state,q))
        .await.map_err(|_| ApiError::bad_request("Otevření obrázku selhalo."))??;
    let mime = sniff(&mut file)?.ok_or_else(|| ApiError::bad_request("Soubor není podporovaný obrázek."))?;
    if size > MAX_IMAGE_BYTES { return Err(ApiError { status:StatusCode::PAYLOAD_TOO_LARGE,
        message:"Obrázek překračuje limit náhledu 256 MiB; můžeš ho stáhnout.".into() }); }
    Response::builder().status(StatusCode::OK)
        .header(header::CONTENT_TYPE,mime)
        .header(header::CONTENT_LENGTH,size.to_string())
        .header(header::CACHE_CONTROL,"no-store")
        .header(header::X_CONTENT_TYPE_OPTIONS,"nosniff")
        .header("content-security-policy","default-src 'none'; sandbox")
        .body(Body::from_stream(ReaderStream::with_capacity(
            tokio::fs::File::from_std(file), 256 * 1024,
        ))).map_err(|_| ApiError::bad_request("Sestavení odpovědi selhalo."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn png_text_and_compressed_metadata() {
        let mut image = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut chunk = |kind: &[u8;4], data: &[u8]| {
            image.extend_from_slice(&(data.len() as u32).to_be_bytes());
            image.extend_from_slice(kind);image.extend_from_slice(data);image.extend_from_slice(&[0u8;4]);
        };
        chunk(b"tEXt",b"parameters\0Steps: 22, Seed: 1234");
        let compressed = {
            use flate2::write::ZlibEncoder;
            use flate2::Compression;
            let mut e=ZlibEncoder::new(Vec::new(),Compression::default());
            e.write_all(b"{\"nodes\":[]}").unwrap();e.finish().unwrap()
        };
        let mut z = b"workflow\0\0".to_vec();z.extend(compressed);
        chunk(b"zTXt",&z);
        chunk(b"IEND",b"");
        let dir=tempfile::tempdir().unwrap();
        let path=dir.path().join("x.png");std::fs::write(&path,image).unwrap();
        let info=preview_info(File::open(path).unwrap(),111).unwrap();
        assert_eq!(info.kind,"image");
        assert!(info.metadata.iter().any(|f|f.key=="parameters"));
        assert!(info.metadata.iter().any(|f|f.key=="workflow" && f.text.as_deref()==Some("{\"nodes\":[]}")));
    }
    #[test]
    fn rejects_binary_as_text() {
        let dir=tempfile::tempdir().unwrap();
        let p=dir.path().join("binary");std::fs::write(&p,[0,1,2,3]).unwrap();
        assert!(preview_info(File::open(p).unwrap(),4).is_err());
    }
}
