//! MTP (Media Transfer Protocol) responder: what makes bman show up on a PC
//! like a phone or a music player, with only its top folders (Music, Videos,
//! Podcasts, Audiobooks). Those are bman's real folders (no disk image), so
//! bman keeps playing while files are copied.
//!
//! The protocol is PTP's (ISO 15740) with Microsoft's MTP additions: 12-byte
//! containers (length, type, code, transaction id) then parameters or data,
//! over a bulk OUT / bulk IN endpoint pair. `usb.rs` provides those through
//! FunctionFS; this file only knows the `Transport` trait, so tests drive it
//! with a fake host.
//!
//! Supported: browsing, downloading (whole or partial), uploading, new
//! folders, delete, rename, move and copy. Uploads are written to a hidden
//! `.bman-upload-*` file and renamed when complete, so the library scanner
//! (which skips hidden files) never sees half-copied songs. Files dropped on
//! the device itself (outside the top folders) go into Music, or Videos for
//! video files; the top folders themselves can't be deleted or renamed.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

/// The bulk endpoints, as the protocol sees them.
pub trait Transport {
    /// Part of a bulk OUT transfer. Fewer bytes than `buf.len()` means the
    /// host ended the transfer (short packet); 0 is a zero-length packet.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    /// Part of a bulk IN transfer. Every write but a transfer's last must be
    /// a multiple of the packet size (`CHUNK` is).
    fn write(&mut self, data: &[u8]) -> io::Result<()>;
    /// wMaxPacketSize of the bulk endpoints at the connected speed.
    fn max_packet(&self) -> usize;
}

/// Transfer size: a multiple of every bulk packet size (64 / 512 / 1024).
pub const CHUNK: usize = 64 * 1024;

const COMMAND: u16 = 1;
const DATA: u16 = 2;
const RESPONSE: u16 = 3;

const STORAGE: u32 = 0x0001_0001;
const ROOT: u32 = 0xFFFF_FFFF;

// Responses
const OK: u16 = 0x2001;
const GENERAL_ERROR: u16 = 0x2002;
const NOT_SUPPORTED: u16 = 0x2005;
const INVALID_STORAGE: u16 = 0x2008;
const INVALID_HANDLE: u16 = 0x2009;
const PROP_NOT_SUPPORTED: u16 = 0x200A;
const STORE_FULL: u16 = 0x200C;
const ACCESS_DENIED: u16 = 0x200F;
const NO_VALID_OBJECT_INFO: u16 = 0x2015;
const INVALID_PARENT: u16 = 0x201A;
const INVALID_PARAMETER: u16 = 0x201D;
const INVALID_PROP_CODE: u16 = 0xA801;
const SPEC_BY_GROUP_UNSUPPORTED: u16 = 0xA807;

// Operations
const GET_DEVICE_INFO: u16 = 0x1001;
const OPEN_SESSION: u16 = 0x1002;
const CLOSE_SESSION: u16 = 0x1003;
const GET_STORAGE_IDS: u16 = 0x1004;
const GET_STORAGE_INFO: u16 = 0x1005;
const GET_NUM_OBJECTS: u16 = 0x1006;
const GET_OBJECT_HANDLES: u16 = 0x1007;
const GET_OBJECT_INFO: u16 = 0x1008;
const GET_OBJECT: u16 = 0x1009;
const DELETE_OBJECT: u16 = 0x100B;
const SEND_OBJECT_INFO: u16 = 0x100C;
const SEND_OBJECT: u16 = 0x100D;
const GET_DEVICE_PROP_DESC: u16 = 0x1014;
const GET_DEVICE_PROP_VALUE: u16 = 0x1015;
const MOVE_OBJECT: u16 = 0x1019;
const COPY_OBJECT: u16 = 0x101A;
const GET_PARTIAL_OBJECT: u16 = 0x101B;
const GET_OBJECT_PROPS_SUPPORTED: u16 = 0x9801;
const GET_OBJECT_PROP_DESC: u16 = 0x9802;
const GET_OBJECT_PROP_VALUE: u16 = 0x9803;
const SET_OBJECT_PROP_VALUE: u16 = 0x9804;
const GET_OBJECT_PROP_LIST: u16 = 0x9805;

const OPERATIONS: [u16; 22] = [
    GET_DEVICE_INFO, OPEN_SESSION, CLOSE_SESSION, GET_STORAGE_IDS, GET_STORAGE_INFO,
    GET_NUM_OBJECTS, GET_OBJECT_HANDLES, GET_OBJECT_INFO, GET_OBJECT, DELETE_OBJECT,
    SEND_OBJECT_INFO, SEND_OBJECT, GET_DEVICE_PROP_DESC, GET_DEVICE_PROP_VALUE, MOVE_OBJECT,
    COPY_OBJECT, GET_PARTIAL_OBJECT, GET_OBJECT_PROPS_SUPPORTED, GET_OBJECT_PROP_DESC,
    GET_OBJECT_PROP_VALUE, SET_OBJECT_PROP_VALUE, GET_OBJECT_PROP_LIST,
];

// Formats
const FMT_UNDEFINED: u16 = 0x3000;
const FMT_FOLDER: u16 = 0x3001;
const FMT_TEXT: u16 = 0x3004;
const FMT_AIFF: u16 = 0x3007;
const FMT_WAV: u16 = 0x3008;
const FMT_MP3: u16 = 0x3009;
const FMT_JPEG: u16 = 0x3801;
const FMT_PNG: u16 = 0x380B;
const FMT_WMA: u16 = 0xB901;
const FMT_OGG: u16 = 0xB902;
const FMT_AAC: u16 = 0xB903;
const FMT_FLAC: u16 = 0xB906;
const FMT_MP4: u16 = 0xB982;
const FMT_M3U: u16 = 0xBA11;
const PLAYBACK_FORMATS: [u16; 14] = [
    FMT_UNDEFINED, FMT_FOLDER, FMT_TEXT, FMT_AIFF, FMT_WAV, FMT_MP3, FMT_JPEG, FMT_PNG,
    FMT_WMA, FMT_OGG, FMT_AAC, FMT_FLAC, FMT_MP4, FMT_M3U,
];

// Object properties and their data types
const PROP_STORAGE_ID: u16 = 0xDC01;
const PROP_FORMAT: u16 = 0xDC02;
const PROP_PROTECTION: u16 = 0xDC03;
const PROP_SIZE: u16 = 0xDC04;
const PROP_FILE_NAME: u16 = 0xDC07;
const PROP_MODIFIED: u16 = 0xDC09;
const PROP_PARENT: u16 = 0xDC0B;
const PROP_UID: u16 = 0xDC41;
const PROP_NAME: u16 = 0xDC44;
const PROPS: [u16; 9] = [
    PROP_STORAGE_ID, PROP_FORMAT, PROP_PROTECTION, PROP_SIZE, PROP_FILE_NAME,
    PROP_MODIFIED, PROP_PARENT, PROP_UID, PROP_NAME,
];
const T_U16: u16 = 0x0004;
const T_U32: u16 = 0x0006;
const T_U64: u16 = 0x0008;
const T_U128: u16 = 0x000A;
const T_STR: u16 = 0xFFFF;

const DEVICE_FRIENDLY_NAME: u16 = 0xD402;

/// Hidden name an upload is written to before it's renamed into place.
const UPLOAD_PREFIX: &str = ".bman-upload-";

const VIDEO_EXTENSIONS: [&str; 6] = ["mp4", "m4v", "mov", "mkv", "webm", "avi"];

struct Obj {
    path: PathBuf,
    /// 0 for the two top folders.
    parent: u32,
}

/// An upload announced by SendObjectInfo, waiting for SendObject.
struct Pending {
    path: PathBuf,
}

/// What a command answers.
enum Reply {
    /// Response code and parameters.
    Done(u16, Vec<u32>),
    /// The host cancelled the transfer: no response is sent.
    Cancelled,
}
use Reply::*;

/// Flags shared with the rest of bman.
#[derive(Clone, Default)]
pub struct Status {
    /// Files were added, removed or renamed (bman rescans; cleared by it).
    pub changed: Arc<AtomicBool>,
    /// A file is being copied right now.
    pub busy: Arc<AtomicBool>,
    /// The host sent a class Cancel request (set from the control endpoint).
    pub cancel: Arc<AtomicBool>,
}

pub struct Mtp {
    /// Name shown for the device.
    name: String,
    serial: String,
    version: String,
    objects: HashMap<u32, Obj>,
    by_path: HashMap<PathBuf, u32>,
    next_handle: u32,
    /// Handles of the top folders (Music, Videos), and their shown names.
    tops: Vec<u32>,
    top_names: Vec<String>,
    pending: Option<Pending>,
    /// A command read while a cancelled data phase was being drained.
    leftover: Option<Vec<u8>>,
    status: Status,
}

impl Mtp {
    /// `folders`: (name shown, folder on disk) for each top folder.
    pub fn new(name: &str, version: &str, folders: &[(&str, PathBuf)], status: Status) -> Self {
        let serial = machine_id(32);
        let mut mtp = Self {
            name: name.into(),
            serial,
            version: version.into(),
            objects: HashMap::new(),
            by_path: HashMap::new(),
            next_handle: 1,
            tops: Vec::new(),
            top_names: folders.iter().map(|(n, _)| n.to_string()).collect(),
            pending: None,
            leftover: None,
            status,
        };
        for (_, path) in folders {
            let _ = fs::create_dir_all(path);
            let handle = mtp.next_handle;
            mtp.next_handle += 1;
            mtp.objects.insert(handle, Obj { path: path.clone(), parent: 0 });
            mtp.by_path.insert(path.clone(), handle);
            mtp.tops.push(handle);
        }
        mtp
    }

    /// Forget a half-done upload after a disconnect.
    pub fn reset(&mut self) {
        self.pending = None;
        self.leftover = None;
    }

    /// Answer commands until the transport fails (unplugged / disabled).
    pub fn serve(&mut self, t: &mut dyn Transport) -> io::Result<()> {
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = match self.leftover.take() {
                Some(cmd) => {
                    buf[..cmd.len()].copy_from_slice(&cmd);
                    cmd.len()
                }
                None => t.read(&mut buf)?,
            };
            if n < 12 {
                continue; // zero-length packet ending an earlier transfer, or junk
            }
            let len = (le32(&buf, 0) as usize).clamp(12, n);
            if le16(&buf, 4) != COMMAND {
                continue; // stray data from a transfer we gave up on
            }
            let code = le16(&buf, 6);
            let tid = le32(&buf, 8);
            let params: Vec<u32> = buf[12..len].chunks_exact(4).map(|c| le32(c, 0)).collect();
            self.status.cancel.store(false, Ordering::Relaxed);
            let reply = self.command(t, code, tid, &params);
            self.status.busy.store(false, Ordering::Relaxed);
            match reply? {
                Done(rc, out) => {
                    if rc != OK {
                        info!("MTP {code:04x} {params:x?}: {rc:04x}");
                    }
                    t.write(&container(RESPONSE, rc, tid, &out))?;
                }
                Cancelled => info!("MTP {code:04x}: cancelled by the host"),
            }
        }
    }

    fn command(&mut self, t: &mut dyn Transport, code: u16, tid: u32, p: &[u32]) -> io::Result<Reply> {
        let arg = |i: usize| p.get(i).copied().unwrap_or(0);
        Ok(match code {
            GET_DEVICE_INFO => {
                let data = self.device_info();
                self.send(t, code, tid, &data)?
            }
            OPEN_SESSION | CLOSE_SESSION => {
                self.pending = None;
                Done(OK, vec![])
            }
            GET_STORAGE_IDS => {
                let mut d = Vec::new();
                put_u32s(&mut d, &[STORAGE]);
                self.send(t, code, tid, &d)?
            }
            GET_STORAGE_INFO => {
                if arg(0) != STORAGE {
                    return Ok(Done(INVALID_STORAGE, vec![]));
                }
                let data = self.storage_info();
                self.send(t, code, tid, &data)?
            }
            GET_NUM_OBJECTS | GET_OBJECT_HANDLES => match self.list(arg(0), arg(1), arg(2)) {
                Err(rc) => Done(rc, vec![]),
                Ok(handles) if code == GET_NUM_OBJECTS => Done(OK, vec![handles.len() as u32]),
                Ok(handles) => {
                    let mut d = Vec::new();
                    put_u32s(&mut d, &handles);
                    self.send(t, code, tid, &d)?
                }
            },
            GET_OBJECT_INFO => match self.object_info(arg(0)) {
                Some(data) => self.send(t, code, tid, &data)?,
                None => Done(INVALID_HANDLE, vec![]),
            },
            GET_OBJECT | GET_PARTIAL_OBJECT => {
                let Some(path) = self.file_path(arg(0)) else {
                    return Ok(Done(INVALID_HANDLE, vec![]));
                };
                let Ok(mut file) = fs::File::open(&path) else {
                    return Ok(Done(ACCESS_DENIED, vec![]));
                };
                let size = file.metadata().map(|m| m.len()).unwrap_or(0);
                let (offset, len) = if code == GET_PARTIAL_OBJECT {
                    let offset = (arg(1) as u64).min(size);
                    (offset, (arg(2) as u64).min(size - offset))
                } else {
                    (0, size)
                };
                if offset > 0 {
                    file.seek(SeekFrom::Start(offset))?;
                }
                self.status.busy.store(true, Ordering::Relaxed);
                match self.send_stream(t, code, tid, len, &mut file)? {
                    Cancelled => Cancelled,
                    _ if code == GET_PARTIAL_OBJECT => Done(OK, vec![len as u32]),
                    _ => Done(OK, vec![]),
                }
            }
            DELETE_OBJECT => self.delete(arg(0)),
            SEND_OBJECT_INFO => self.send_object_info(t, arg(0), arg(1))?,
            SEND_OBJECT => self.send_object(t)?,
            GET_DEVICE_PROP_DESC | GET_DEVICE_PROP_VALUE => {
                if arg(0) != DEVICE_FRIENDLY_NAME as u32 {
                    return Ok(Done(PROP_NOT_SUPPORTED, vec![]));
                }
                let mut d = Vec::new();
                if code == GET_DEVICE_PROP_DESC {
                    put_u16(&mut d, DEVICE_FRIENDLY_NAME);
                    put_u16(&mut d, T_STR);
                    d.push(0); // read-only
                    put_str(&mut d, &self.name);
                    put_str(&mut d, &self.name);
                    d.push(0); // no form
                } else {
                    put_str(&mut d, &self.name);
                }
                self.send(t, code, tid, &d)?
            }
            MOVE_OBJECT | COPY_OBJECT => self.move_or_copy(code == COPY_OBJECT, arg(0), arg(1), arg(2)),
            GET_OBJECT_PROPS_SUPPORTED => {
                let mut d = Vec::new();
                put_u32(&mut d, PROPS.len() as u32);
                for p in PROPS {
                    put_u16(&mut d, p);
                }
                self.send(t, code, tid, &d)?
            }
            GET_OBJECT_PROP_DESC => {
                let prop = arg(0) as u16;
                let Some(kind) = prop_type(prop) else {
                    return Ok(Done(INVALID_PROP_CODE, vec![]));
                };
                let mut d = Vec::new();
                put_u16(&mut d, prop);
                put_u16(&mut d, kind);
                d.push(u8::from(prop == PROP_FILE_NAME)); // only the name can be set
                put_default(&mut d, kind);
                put_u32(&mut d, 0); // group
                d.push(0); // no form
                self.send(t, code, tid, &d)?
            }
            GET_OBJECT_PROP_VALUE => {
                let (handle, prop) = (arg(0), arg(1) as u16);
                if prop_type(prop).is_none() {
                    return Ok(Done(INVALID_PROP_CODE, vec![]));
                }
                let mut d = Vec::new();
                if !self.put_prop(&mut d, handle, prop) {
                    return Ok(Done(INVALID_HANDLE, vec![]));
                }
                self.send(t, code, tid, &d)?
            }
            SET_OBJECT_PROP_VALUE => self.set_prop(t, arg(0), arg(1) as u16)?,
            GET_OBJECT_PROP_LIST => self.prop_list(t, tid, p)?,
            _ => Done(NOT_SUPPORTED, vec![]),
        })
    }

    // --- objects ------------------------------------------------------------

    fn handle_of(&mut self, path: &Path, parent: u32) -> u32 {
        if let Some(&h) = self.by_path.get(path) {
            return h;
        }
        let h = self.next_handle;
        self.next_handle += 1;
        self.objects.insert(h, Obj { path: path.to_path_buf(), parent });
        self.by_path.insert(path.to_path_buf(), h);
        h
    }

    /// An existing object's path.
    fn path(&self, handle: u32) -> Option<PathBuf> {
        let obj = self.objects.get(&handle)?;
        fs::symlink_metadata(&obj.path).ok().map(|_| obj.path.clone())
    }

    fn file_path(&self, handle: u32) -> Option<PathBuf> {
        self.path(handle).filter(|p| p.is_file())
    }

    fn children(&mut self, handle: u32) -> Vec<u32> {
        let Some(dir) = self.path(handle).filter(|p| p.is_dir()) else {
            return Vec::new();
        };
        let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
                    .map(|e| e.path())
                    .filter(|p| p.is_file() || p.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        paths.iter().map(|p| self.handle_of(p, handle)).collect()
    }

    /// GetObjectHandles: `parent` 0xFFFFFFFF = the top level, 0 = everything.
    fn list(&mut self, storage: u32, format: u32, parent: u32) -> Result<Vec<u32>, u16> {
        if storage != STORAGE && storage != ROOT {
            return Err(INVALID_STORAGE);
        }
        let mut handles = match parent {
            ROOT => self.tops.clone(),
            0 => {
                let mut all = Vec::new();
                let mut stack = self.tops.clone();
                while let Some(h) = stack.pop() {
                    all.push(h);
                    stack.extend(self.children(h));
                }
                all
            }
            h if self.path(h).is_some_and(|p| p.is_dir()) => self.children(h),
            _ => return Err(INVALID_PARENT),
        };
        if format != 0 {
            handles.retain(|&h| self.format(h) == format as u16);
        }
        Ok(handles)
    }

    fn format(&self, handle: u32) -> u16 {
        match self.objects.get(&handle) {
            Some(o) if o.path.is_dir() => FMT_FOLDER,
            Some(o) => format_of(&o.path),
            None => FMT_UNDEFINED,
        }
    }

    /// Name shown for an object (the top folders are "Music" / "Videos").
    fn name(&self, handle: u32) -> String {
        if let Some(i) = self.tops.iter().position(|&h| h == handle) {
            return self.top_names[i].clone();
        }
        self.objects
            .get(&handle)
            .and_then(|o| o.path.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    fn object_info(&self, handle: u32) -> Option<Vec<u8>> {
        let path = self.path(handle)?;
        let meta = fs::metadata(&path).ok()?;
        let parent = self.objects[&handle].parent;
        let mut d = Vec::new();
        put_u32(&mut d, STORAGE);
        put_u16(&mut d, self.format(handle));
        put_u16(&mut d, 0); // protection
        put_u32(&mut d, if meta.is_dir() { 0 } else { meta.len().min(u32::MAX as u64) as u32 });
        put_u16(&mut d, 0); // thumb format
        for _ in 0..6 {
            put_u32(&mut d, 0); // thumb size/width/height, image width/height/depth
        }
        put_u32(&mut d, parent);
        put_u16(&mut d, u16::from(meta.is_dir())); // association: generic folder
        put_u32(&mut d, 0);
        put_u32(&mut d, 0); // sequence number
        put_str(&mut d, &self.name(handle));
        let date = date_string(meta.modified().ok());
        put_str(&mut d, &date); // created
        put_str(&mut d, &date); // modified
        put_str(&mut d, ""); // keywords
        Some(d)
    }

    fn is_top(&self, handle: u32) -> bool {
        self.tops.contains(&handle)
    }

    /// Point every object at or under `old` to the same place under `new`.
    fn repath(&mut self, old: &Path, new: &Path) {
        let moved: Vec<(u32, PathBuf)> = self
            .objects
            .iter()
            .filter_map(|(&h, o)| {
                // join("") would add a trailing slash.
                let rest = o.path.strip_prefix(old).ok()?;
                Some((h, if rest.as_os_str().is_empty() { new.to_path_buf() } else { new.join(rest) }))
            })
            .collect();
        for (h, path) in moved {
            if let Some(obj) = self.objects.get_mut(&h) {
                self.by_path.remove(&obj.path);
                obj.path = path.clone();
                self.by_path.insert(path, h);
            }
        }
    }

    /// Forget every object at or under `path`.
    fn forget(&mut self, path: &Path) {
        let gone: Vec<u32> = self
            .objects
            .iter()
            .filter(|(_, o)| o.path.starts_with(path))
            .map(|(&h, _)| h)
            .collect();
        for h in gone {
            if let Some(obj) = self.objects.remove(&h) {
                self.by_path.remove(&obj.path);
            }
        }
    }

    fn changed(&self) {
        self.status.changed.store(true, Ordering::Relaxed);
    }

    fn delete(&mut self, handle: u32) -> Reply {
        if handle == ROOT || self.is_top(handle) {
            return Done(ACCESS_DENIED, vec![]);
        }
        let Some(path) = self.path(handle) else {
            return Done(INVALID_HANDLE, vec![]);
        };
        let result = if path.is_dir() && !path.is_symlink() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        if let Err(e) = result {
            warn!("MTP: couldn't delete {}: {e}", path.display());
            return Done(ACCESS_DENIED, vec![]);
        }
        info!("MTP: deleted {}", path.display());
        self.forget(&path);
        self.changed();
        Done(OK, vec![])
    }

    /// Where something put in `parent` goes: a top folder or a folder in
    /// one. Things put on the device itself go to Music, videos to Videos.
    fn target_dir(&self, parent: u32, name: &str) -> Option<(u32, PathBuf)> {
        let parent = if parent == ROOT || parent == 0 {
            let video = Path::new(name)
                .extension()
                .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.to_string_lossy().to_lowercase().as_str()));
            let i = if video && self.tops.len() > 1 { 1 } else { 0 };
            *self.tops.get(i)?
        } else {
            parent
        };
        self.path(parent).filter(|p| p.is_dir()).map(|p| (parent, p))
    }

    fn move_or_copy(&mut self, copy: bool, handle: u32, storage: u32, parent: u32) -> Reply {
        if storage != STORAGE {
            return Done(INVALID_STORAGE, vec![]);
        }
        if !copy && self.is_top(handle) {
            return Done(ACCESS_DENIED, vec![]);
        }
        let Some(from) = self.path(handle) else {
            return Done(INVALID_HANDLE, vec![]);
        };
        let name = self.name(handle);
        let Some((parent, dir)) = self.target_dir(parent, &name) else {
            return Done(INVALID_PARENT, vec![]);
        };
        let to = dir.join(&name);
        if to.starts_with(&from) {
            return Done(INVALID_PARENT, vec![]); // into itself
        }
        let result = if copy { copy_all(&from, &to) } else { move_path(&from, &to) };
        if let Err(e) = result {
            warn!("MTP: {} {} -> {}: {e}", if copy { "copy" } else { "move" }, from.display(), to.display());
            return Done(if is_full(&e) { STORE_FULL } else { GENERAL_ERROR }, vec![]);
        }
        self.changed();
        if copy {
            let h = self.handle_of(&to, parent);
            Done(OK, vec![h])
        } else {
            self.repath(&from, &to);
            if let Some(obj) = self.objects.get_mut(&handle) {
                obj.parent = parent;
            }
            Done(OK, vec![])
        }
    }

    fn send_object_info(&mut self, t: &mut dyn Transport, storage: u32, parent: u32) -> io::Result<Reply> {
        let Some(data) = self.recv(t)? else { return Ok(Cancelled) };
        self.pending = None;
        if storage != STORAGE && storage != 0 {
            return Ok(Done(INVALID_STORAGE, vec![]));
        }
        if data.len() < 53 {
            return Ok(Done(INVALID_PARAMETER, vec![]));
        }
        let format = le16(&data, 4);
        let Some(name) = read_str(&data, 52).filter(|n| valid_name(n)) else {
            return Ok(Done(INVALID_PARAMETER, vec![]));
        };
        let Some((parent, dir)) = self.target_dir(parent, &name) else {
            return Ok(Done(INVALID_PARENT, vec![]));
        };
        let path = dir.join(&name);
        if format == FMT_FOLDER {
            if let Err(e) = fs::create_dir_all(&path) {
                warn!("MTP: couldn't make {}: {e}", path.display());
                return Ok(Done(if is_full(&e) { STORE_FULL } else { ACCESS_DENIED }, vec![]));
            }
            info!("MTP: new folder {}", path.display());
            self.changed();
            let h = self.handle_of(&path, parent);
            return Ok(Done(OK, vec![STORAGE, parent, h]));
        }
        // An upload over an existing file replaces it.
        let h = self.handle_of(&path, parent);
        self.pending = Some(Pending { path });
        Ok(Done(OK, vec![STORAGE, parent, h]))
    }

    fn send_object(&mut self, t: &mut dyn Transport) -> io::Result<Reply> {
        let Some(pending) = self.pending.take() else {
            // Still read the data the host sends, then say no.
            return Ok(match self.recv_with(t, &mut |_| Ok(()))? {
                None => Cancelled,
                Some(_) => Done(NO_VALID_OBJECT_INFO, vec![]),
            });
        };
        let name = pending.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let tmp = pending.path.with_file_name(format!("{UPLOAD_PREFIX}{name}"));
        let mut file = match fs::File::create(&tmp) {
            Ok(f) => Some(f),
            Err(e) => {
                warn!("MTP: couldn't create {}: {e}", tmp.display());
                None
            }
        };
        self.status.busy.store(true, Ordering::Relaxed);
        let mut error: Option<io::Error> = None;
        // A failed write still reads (and drops) the rest of the data.
        let got = self.recv_with(t, &mut |chunk| {
            if let (Some(f), None) = (file.as_mut(), error.as_ref()) {
                if let Err(e) = f.write_all(chunk) {
                    error = Some(e);
                }
            }
            Ok(())
        })?;
        if got.is_none() {
            let _ = fs::remove_file(&tmp);
            return Ok(Cancelled);
        }
        let Some(file) = file else {
            return Ok(Done(ACCESS_DENIED, vec![]));
        };
        let result = match error {
            Some(e) => Err(e),
            None => file.sync_all().and_then(|()| fs::rename(&tmp, &pending.path)),
        };
        match result {
            Ok(()) => {
                info!("MTP: received {}", pending.path.display());
                self.changed();
                Ok(Done(OK, vec![]))
            }
            Err(e) => {
                warn!("MTP: couldn't write {}: {e}", pending.path.display());
                let _ = fs::remove_file(&tmp);
                Ok(Done(if is_full(&e) { STORE_FULL } else { GENERAL_ERROR }, vec![]))
            }
        }
    }

    // --- properties -------------------------------------------------------------

    /// Append `prop` of `handle` to `d`; false if there's no such object.
    fn put_prop(&self, d: &mut Vec<u8>, handle: u32, prop: u16) -> bool {
        let Some(path) = self.path(handle) else { return false };
        let meta = fs::metadata(&path).ok();
        match prop {
            PROP_STORAGE_ID => put_u32(d, STORAGE),
            PROP_FORMAT => put_u16(d, self.format(handle)),
            PROP_PROTECTION => put_u16(d, 0),
            PROP_SIZE => put_u64(d, meta.filter(|m| m.is_file()).map_or(0, |m| m.len())),
            PROP_FILE_NAME | PROP_NAME => put_str(d, &self.name(handle)),
            PROP_MODIFIED => put_str(d, &date_string(meta.and_then(|m| m.modified().ok()))),
            PROP_PARENT => put_u32(d, self.objects[&handle].parent),
            PROP_UID => {
                put_u64(d, handle as u64);
                put_u64(d, 0);
            }
            _ => return false,
        }
        true
    }

    fn set_prop(&mut self, t: &mut dyn Transport, handle: u32, prop: u16) -> io::Result<Reply> {
        let Some(data) = self.recv(t)? else { return Ok(Cancelled) };
        if prop != PROP_FILE_NAME && prop != PROP_NAME {
            return Ok(Done(if prop_type(prop).is_some() { ACCESS_DENIED } else { INVALID_PROP_CODE }, vec![]));
        }
        if self.is_top(handle) {
            return Ok(Done(ACCESS_DENIED, vec![]));
        }
        let Some(path) = self.path(handle) else { return Ok(Done(INVALID_HANDLE, vec![])) };
        let Some(name) = read_str(&data, 0).filter(|n| valid_name(n)) else {
            return Ok(Done(INVALID_PARAMETER, vec![]));
        };
        let to = path.with_file_name(&name);
        if to != path {
            if let Err(e) = move_path(&path, &to) {
                warn!("MTP: rename {} -> {name}: {e}", path.display());
                return Ok(Done(ACCESS_DENIED, vec![]));
            }
            info!("MTP: renamed {} to {name}", path.display());
            self.repath(&path, &to);
            self.changed();
        }
        Ok(Done(OK, vec![]))
    }

    /// GetObjectPropList(handle, format, prop, group, depth).
    fn prop_list(&mut self, t: &mut dyn Transport, tid: u32, p: &[u32]) -> io::Result<Reply> {
        let arg = |i: usize| p.get(i).copied().unwrap_or(0);
        let (handle, format, prop, group, depth) = (arg(0), arg(1), arg(2), arg(3), arg(4));
        if prop == 0 && group != 0 {
            return Ok(Done(SPEC_BY_GROUP_UNSUPPORTED, vec![]));
        }
        let props: Vec<u16> = match prop {
            0xFFFF_FFFF => PROPS.to_vec(),
            p if prop_type(p as u16).is_some() => vec![p as u16],
            _ => return Ok(Done(INVALID_PROP_CODE, vec![])),
        };
        let mut handles = match (handle, depth) {
            (0 | ROOT, 0) => Vec::new(),
            (h, 0) => vec![h],
            (0 | ROOT, 1) => self.tops.clone(),
            (0 | ROOT, _) => match self.list(STORAGE, 0, 0) {
                Ok(all) => all,
                Err(rc) => return Ok(Done(rc, vec![])),
            },
            (h, _) => {
                if self.path(h).is_none() {
                    return Ok(Done(INVALID_HANDLE, vec![]));
                }
                self.children(h)
            }
        };
        if format != 0 {
            handles.retain(|&h| self.format(h) == format as u16);
        }
        let mut d = Vec::new();
        let mut count = 0u32;
        put_u32(&mut d, 0);
        for &h in &handles {
            for &pc in &props {
                let mark = d.len();
                put_u32(&mut d, h);
                put_u16(&mut d, pc);
                put_u16(&mut d, prop_type(pc).unwrap_or(T_STR));
                if self.put_prop(&mut d, h, pc) {
                    count += 1;
                } else {
                    d.truncate(mark);
                    if handles.len() == 1 {
                        return Ok(Done(INVALID_HANDLE, vec![]));
                    }
                }
            }
        }
        d[..4].copy_from_slice(&count.to_le_bytes());
        self.send(t, GET_OBJECT_PROP_LIST, tid, &d)
    }

    // --- datasets -------------------------------------------------------------

    fn device_info(&self) -> Vec<u8> {
        let mut d = Vec::new();
        put_u16(&mut d, 100); // PTP 1.00
        put_u32(&mut d, 6); // Microsoft's extensions (MTP)
        put_u16(&mut d, 100);
        put_str(&mut d, "microsoft.com: 1.0;");
        put_u16(&mut d, 0); // functional mode
        put_u32(&mut d, OPERATIONS.len() as u32);
        for op in OPERATIONS {
            put_u16(&mut d, op);
        }
        put_u32(&mut d, 0); // events
        put_u32(&mut d, 1);
        put_u16(&mut d, DEVICE_FRIENDLY_NAME);
        put_u32(&mut d, 0); // capture formats
        put_u32(&mut d, PLAYBACK_FORMATS.len() as u32);
        for f in PLAYBACK_FORMATS {
            put_u16(&mut d, f);
        }
        put_str(&mut d, "bman");
        put_str(&mut d, &self.name);
        put_str(&mut d, &self.version);
        put_str(&mut d, &self.serial);
        d
    }

    fn storage_info(&self) -> Vec<u8> {
        let (total, free) = self
            .tops
            .first()
            .and_then(|h| self.objects.get(h))
            .map_or((0, 0), |o| disk_space(&o.path));
        let mut d = Vec::new();
        put_u16(&mut d, 0x0003); // fixed RAM
        put_u16(&mut d, 0x0002); // generic hierarchical
        put_u16(&mut d, 0x0000); // read-write
        put_u64(&mut d, total);
        put_u64(&mut d, free);
        put_u32(&mut d, 0xFFFF_FFFF);
        put_str(&mut d, &self.name);
        put_str(&mut d, &self.serial);
        d
    }

    // --- data phases ------------------------------------------------------------

    fn send(&mut self, t: &mut dyn Transport, code: u16, tid: u32, data: &[u8]) -> io::Result<Reply> {
        Ok(match self.send_stream(t, code, tid, data.len() as u64, &mut &data[..])? {
            Cancelled => Cancelled,
            _ => Done(OK, vec![]),
        })
    }

    /// Data phase to the host: `len` bytes from `src`, in CHUNK writes, then a
    /// zero-length packet if the transfer is a whole number of packets.
    fn send_stream(&mut self, t: &mut dyn Transport, code: u16, tid: u32, len: u64, src: &mut dyn Read) -> io::Result<Reply> {
        let total = 12 + len;
        let mut buf = vec![0u8; CHUNK.min(total as usize)];
        let header = container(DATA, code, tid, &[]);
        buf[..12].copy_from_slice(&header);
        let len32 = if total > u32::MAX as u64 { u32::MAX } else { total as u32 };
        buf[..4].copy_from_slice(&len32.to_le_bytes());
        let mut pos = 12;
        let mut left = len;
        loop {
            let want = ((buf.len() - pos) as u64).min(left) as usize;
            let part = &mut buf[pos..pos + want];
            if let Err(e) = src.read_exact(part) {
                // The file changed under us: the length is promised, so pad.
                warn!("MTP: read failed mid-transfer: {e}");
                part.fill(0);
            }
            left -= want as u64;
            t.write(&buf[..pos + want])?;
            pos = 0;
            if self.status.cancel.load(Ordering::Relaxed) {
                return Ok(Cancelled);
            }
            if left == 0 {
                break;
            }
        }
        if total % t.max_packet() as u64 == 0 {
            t.write(&[])?;
        }
        Ok(Done(OK, vec![]))
    }

    /// A data phase from the host, whole (for small datasets).
    fn recv(&mut self, t: &mut dyn Transport) -> io::Result<Option<Vec<u8>>> {
        let mut data = Vec::new();
        let got = self.recv_with(t, &mut |chunk| {
            if data.len() < 1 << 20 {
                data.extend_from_slice(chunk);
            }
            Ok(())
        })?;
        Ok(got.map(|_| data))
    }

    /// A data phase from the host, given to `sink` as it arrives. None: it
    /// was cancelled (or the host sent the next command instead).
    fn recv_with(&mut self, t: &mut dyn Transport, sink: &mut dyn FnMut(&[u8]) -> io::Result<()>) -> io::Result<Option<u64>> {
        let mut buf = vec![0u8; CHUNK];
        let mut n = 0;
        while n == 0 {
            n = t.read(&mut buf)?; // skip a zero-length packet left over
        }
        if self.status.cancel.load(Ordering::Relaxed) || n < 12 || le16(&buf, 4) != DATA {
            if n >= 12 && le16(&buf, 4) == COMMAND {
                self.leftover = Some(buf[..n].to_vec());
            }
            return Ok(None);
        }
        let len = le32(&buf, 0);
        let expected = (len != u32::MAX).then(|| (len as u64).saturating_sub(12));
        let first = n.min(12 + expected.unwrap_or(u64::MAX).min(CHUNK as u64) as usize);
        sink(&buf[12..first])?;
        let mut got = (first - 12) as u64;
        let mut ended = n < buf.len();
        loop {
            let done = match expected {
                Some(e) => got >= e,
                None => ended,
            };
            if done {
                return Ok(Some(got));
            }
            let n = t.read(&mut buf)?;
            if self.status.cancel.load(Ordering::Relaxed) {
                if n >= 12 && le16(&buf, 4) == COMMAND {
                    self.leftover = Some(buf[..n].to_vec());
                }
                return Ok(None);
            }
            sink(&buf[..n])?;
            got += n as u64;
            ended = n < buf.len();
            if ended && expected.is_some_and(|e| got < e) {
                // Short transfer: the host gave up.
                return Ok(Some(got));
            }
        }
    }
}

// --- helpers -----------------------------------------------------------------

/// The first `len` characters of the machine id ("bman" if there's none):
/// a serial number that stays the same across restarts.
pub fn machine_id(len: usize) -> String {
    ["/etc/machine-id", "/var/lib/dbus/machine-id"]
        .iter()
        .filter_map(|p| fs::read_to_string(p).ok())
        .map(|s| s.trim().chars().take(len).collect::<String>())
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "bman".into())
}

fn container(kind: u16, code: u16, tid: u32, params: &[u32]) -> Vec<u8> {
    let mut c = Vec::with_capacity(12 + 4 * params.len());
    put_u32(&mut c, (12 + 4 * params.len()) as u32);
    put_u16(&mut c, kind);
    put_u16(&mut c, code);
    put_u32(&mut c, tid);
    for &p in params {
        put_u32(&mut c, p);
    }
    c
}

fn le16(b: &[u8], at: usize) -> u16 {
    b.get(at..at + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}

fn le32(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn put_u16(d: &mut Vec<u8>, v: u16) {
    d.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(d: &mut Vec<u8>, v: u32) {
    d.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(d: &mut Vec<u8>, v: u64) {
    d.extend_from_slice(&v.to_le_bytes());
}

fn put_u32s(d: &mut Vec<u8>, v: &[u32]) {
    put_u32(d, v.len() as u32);
    for &x in v {
        put_u32(d, x);
    }
}

/// PTP string: character count (with the terminating 0) then UTF-16LE.
fn put_str(d: &mut Vec<u8>, s: &str) {
    if s.is_empty() {
        d.push(0);
        return;
    }
    let units: Vec<u16> = s.encode_utf16().take(254).collect();
    d.push(units.len() as u8 + 1);
    for u in units {
        put_u16(d, u);
    }
    put_u16(d, 0);
}

fn read_str(d: &[u8], at: usize) -> Option<String> {
    let count = *d.get(at)? as usize;
    let units: Vec<u16> = (0..count).map(|i| le16(d, at + 1 + 2 * i)).take_while(|&u| u != 0).collect();
    String::from_utf16(&units).ok()
}

fn prop_type(prop: u16) -> Option<u16> {
    Some(match prop {
        PROP_STORAGE_ID | PROP_PARENT => T_U32,
        PROP_FORMAT | PROP_PROTECTION => T_U16,
        PROP_SIZE => T_U64,
        PROP_UID => T_U128,
        PROP_FILE_NAME | PROP_MODIFIED | PROP_NAME => T_STR,
        _ => return None,
    })
}

fn put_default(d: &mut Vec<u8>, kind: u16) {
    match kind {
        T_U16 => put_u16(d, 0),
        T_U32 => put_u32(d, 0),
        T_U64 => put_u64(d, 0),
        T_U128 => d.extend_from_slice(&[0; 16]),
        _ => d.push(0),
    }
}

fn format_of(path: &Path) -> u16 {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "mp3" => FMT_MP3,
        "wav" => FMT_WAV,
        "aif" | "aiff" => FMT_AIFF,
        "flac" => FMT_FLAC,
        "ogg" | "oga" | "opus" => FMT_OGG,
        "aac" | "m4a" => FMT_AAC,
        "wma" => FMT_WMA,
        "mp4" | "m4v" | "mov" => FMT_MP4,
        "m3u" | "m3u8" => FMT_M3U,
        "jpg" | "jpeg" => FMT_JPEG,
        "png" => FMT_PNG,
        "txt" => FMT_TEXT,
        _ => FMT_UNDEFINED,
    }
}

/// A plain file name: no paths, nothing hidden by bman's own temp names.
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\0'])
        && !name.starts_with(UPLOAD_PREFIX)
        && name.len() <= 255
}

fn is_full(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::ENOSPC | libc::EDQUOT))
}

/// Rename, or copy and delete across filesystems (music/ may be a symlink
/// to another disk).
fn move_path(from: &Path, to: &Path) -> io::Result<()> {
    match fs::rename(from, to) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            copy_all(from, to)?;
            if from.is_dir() { fs::remove_dir_all(from) } else { fs::remove_file(from) }
        }
        other => other,
    }
}

fn copy_all(from: &Path, to: &Path) -> io::Result<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)?.flatten() {
            copy_all(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        fs::copy(from, to).map(|_| ())
    }
}

/// (total, free) bytes of the filesystem `path` is on.
fn disk_space(path: &Path) -> (u64, u64) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return (0, 0) };
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return (0, 0);
    }
    let frsize = s.f_frsize as u64;
    (s.f_blocks as u64 * frsize, s.f_bavail as u64 * frsize)
}

/// "YYYYMMDDThhmmss" in local time.
fn date_string(t: Option<SystemTime>) -> String {
    let Some(secs) = t.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()) else {
        return String::new();
    };
    let secs = secs.as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        return String::new();
    }
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A host: queued bulk OUT transfers (already split as reads would see
    /// them), and everything the device wrote.
    struct FakeHost {
        out: VecDeque<Vec<u8>>,
        written: Vec<Vec<u8>>,
    }

    impl Transport for FakeHost {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut next = self.out.pop_front().ok_or_else(|| io::Error::other("unplugged"))?;
            let n = next.len().min(buf.len());
            buf[..n].copy_from_slice(&next[..n]);
            if n < next.len() {
                self.out.push_front(next.split_off(n));
            }
            Ok(n)
        }
        fn write(&mut self, data: &[u8]) -> io::Result<()> {
            self.written.push(data.to_vec());
            Ok(())
        }
        fn max_packet(&self) -> usize {
            512
        }
    }

    fn cmd(code: u16, tid: u32, params: &[u32]) -> Vec<u8> {
        container(COMMAND, code, tid, params)
    }

    /// A data container, split into CHUNK-sized reads.
    fn data(code: u16, tid: u32, payload: &[u8]) -> Vec<Vec<u8>> {
        let mut c = Vec::new();
        put_u32(&mut c, (12 + payload.len()) as u32);
        put_u16(&mut c, DATA);
        put_u16(&mut c, code);
        put_u32(&mut c, tid);
        c.extend_from_slice(payload);
        let mut parts: Vec<Vec<u8>> = c.chunks(CHUNK).map(<[u8]>::to_vec).collect();
        if c.len() % 512 == 0 {
            parts.push(Vec::new()); // zero-length packet
        }
        parts
    }

    fn object_info(name: &str, format: u16, size: u32) -> Vec<u8> {
        let mut d = Vec::new();
        put_u32(&mut d, STORAGE);
        put_u16(&mut d, format);
        put_u16(&mut d, 0);
        put_u32(&mut d, size);
        put_u16(&mut d, 0);
        for _ in 0..6 {
            put_u32(&mut d, 0);
        }
        put_u32(&mut d, 0);
        put_u16(&mut d, 0);
        put_u32(&mut d, 0);
        put_u32(&mut d, 0);
        put_str(&mut d, name);
        for _ in 0..3 {
            put_str(&mut d, "");
        }
        d
    }

    /// Run the host's transfers; returns the device's writes merged into
    /// containers: (type, code, payload).
    fn run(mtp: &mut Mtp, out: Vec<Vec<u8>>) -> Vec<(u16, u16, Vec<u8>)> {
        let mut host = FakeHost { out: out.into(), written: Vec::new() };
        let _ = mtp.serve(&mut host);
        let mut stream = Vec::new();
        for w in host.written {
            stream.extend(w);
        }
        let mut out = Vec::new();
        let mut at = 0;
        while at + 12 <= stream.len() {
            let len = le32(&stream, at) as usize;
            out.push((le16(&stream, at + 4), le16(&stream, at + 6), stream[at + 12..at + len].to_vec()));
            at += len;
        }
        out
    }

    fn setup(name: &str) -> (PathBuf, Mtp) {
        let base = std::env::temp_dir().join(format!("bman-mtp-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let mtp = Mtp::new(
            "bman",
            "0.0.0",
            &[("Music", base.join("music")), ("Videos", base.join("videos"))],
            Status::default(),
        );
        (base, mtp)
    }

    fn response(r: &[(u16, u16, Vec<u8>)]) -> (u16, Vec<u32>) {
        let (_, code, p) = r.iter().rev().find(|(t, _, _)| *t == RESPONSE).unwrap();
        (*code, p.chunks_exact(4).map(|c| le32(c, 0)).collect())
    }

    #[test]
    fn browse_upload_download_rename_delete() {
        let (base, mut mtp) = setup("basic");

        let r = run(&mut mtp, vec![cmd(OPEN_SESSION, 1, &[1]), cmd(GET_OBJECT_HANDLES, 2, &[STORAGE, 0, ROOT])]);
        let (_, _, handles) = r.iter().find(|(t, _, _)| *t == DATA).unwrap();
        assert_eq!(handles, &[2, 0, 0, 0, 1, 0, 0, 0, 2, 0, 0, 0]);
        let r = run(&mut mtp, vec![cmd(GET_OBJECT_INFO, 3, &[1])]);
        assert_eq!(read_str(&r[0].2, 52).unwrap(), "Music");

        // Upload a song into Music.
        let mut out = vec![cmd(SEND_OBJECT_INFO, 4, &[STORAGE, 1])];
        out.extend(data(SEND_OBJECT_INFO, 4, &object_info("a.mp3", FMT_MP3, 5)));
        let r = run(&mut mtp, out);
        let (rc, p) = response(&r);
        assert_eq!(rc, OK);
        assert_eq!(p[..2], [STORAGE, 1]);
        let song = p[2];
        let mut out = vec![cmd(SEND_OBJECT, 5, &[])];
        out.extend(data(SEND_OBJECT, 5, b"hello"));
        assert_eq!(response(&run(&mut mtp, out)).0, OK);
        assert_eq!(fs::read(base.join("music/a.mp3")).unwrap(), b"hello");
        assert_eq!(fs::read_dir(base.join("music")).unwrap().count(), 1); // no temp file left
        assert!(mtp.status.changed.load(Ordering::Relaxed));

        // Download it, whole and part.
        let r = run(&mut mtp, vec![cmd(GET_OBJECT, 6, &[song])]);
        assert_eq!(r[0].2, b"hello");
        let r = run(&mut mtp, vec![cmd(GET_PARTIAL_OBJECT, 7, &[song, 1, 3])]);
        assert_eq!(r[0].2, b"ell");
        assert_eq!(response(&r).1, vec![3]);

        // A video dropped on the device itself goes into Videos.
        let mut out = vec![cmd(SEND_OBJECT_INFO, 8, &[STORAGE, ROOT])];
        out.extend(data(SEND_OBJECT_INFO, 8, &object_info("clip.mp4", FMT_MP4, 1)));
        out.push(cmd(SEND_OBJECT, 9, &[]));
        out.extend(data(SEND_OBJECT, 9, b"v"));
        let r = run(&mut mtp, out);
        assert_eq!(response(&r).0, OK);
        assert!(base.join("videos/clip.mp4").is_file());

        // Rename, then the property list sees the new name.
        let mut name = Vec::new();
        put_str(&mut name, "b.mp3");
        let mut out = vec![cmd(SET_OBJECT_PROP_VALUE, 10, &[song, PROP_FILE_NAME as u32])];
        out.extend(data(SET_OBJECT_PROP_VALUE, 10, &name));
        assert_eq!(response(&run(&mut mtp, out)).0, OK);
        assert!(base.join("music/b.mp3").is_file());
        let r = run(&mut mtp, vec![cmd(GET_OBJECT_PROP_LIST, 11, &[1, 0, PROP_FILE_NAME as u32, 0, 1])]);
        let list = &r[0].2;
        assert_eq!(le32(list, 0), 1);
        assert_eq!(read_str(list, 12).unwrap(), "b.mp3");

        // New folder, move the song into it.
        let mut out = vec![cmd(SEND_OBJECT_INFO, 12, &[STORAGE, 1])];
        out.extend(data(SEND_OBJECT_INFO, 12, &object_info("Album", FMT_FOLDER, 0)));
        let folder = response(&run(&mut mtp, out)).1[2];
        assert!(base.join("music/Album").is_dir());
        assert_eq!(response(&run(&mut mtp, vec![cmd(MOVE_OBJECT, 13, &[song, STORAGE, folder])])).0, OK);
        assert!(base.join("music/Album/b.mp3").is_file());

        // The top folders stay; the rest can go.
        assert_eq!(response(&run(&mut mtp, vec![cmd(DELETE_OBJECT, 14, &[1])])).0, ACCESS_DENIED);
        assert_eq!(response(&run(&mut mtp, vec![cmd(DELETE_OBJECT, 15, &[folder])])).0, OK);
        assert!(!base.join("music/Album").exists());
        assert_eq!(response(&run(&mut mtp, vec![cmd(GET_OBJECT_INFO, 16, &[song])])).0, INVALID_HANDLE);

        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn big_upload_and_download() {
        let (base, mut mtp) = setup("big");
        // 12 + size is a whole number of packets: ends with a zero-length packet.
        let payload: Vec<u8> = (0..(3 * CHUNK - 12)).map(|i| (i % 251) as u8).collect();
        let mut out = vec![cmd(SEND_OBJECT_INFO, 1, &[STORAGE, 1])];
        out.extend(data(SEND_OBJECT_INFO, 1, &object_info("big.flac", FMT_FLAC, payload.len() as u32)));
        out.push(cmd(SEND_OBJECT, 2, &[]));
        out.extend(data(SEND_OBJECT, 2, &payload));
        out.push(cmd(GET_OBJECT_HANDLES, 3, &[STORAGE, 0, 1]));
        let r = run(&mut mtp, out);
        assert_eq!(fs::read(base.join("music/big.flac")).unwrap(), payload);
        // The zero-length packet wasn't taken as a command or data.
        let handles = &r.iter().find(|(t, c, _)| *t == DATA && *c == GET_OBJECT_HANDLES).unwrap().2;
        assert_eq!(le32(handles, 0), 1);
        let h = le32(handles, 4);

        let mut host = FakeHost { out: vec![cmd(GET_OBJECT, 4, &[h])].into(), written: Vec::new() };
        let _ = mtp.serve(&mut host);
        // Writes: CHUNK-sized pieces, a zero-length packet, then the response.
        assert!(host.written[..3].iter().all(|w| w.len() == CHUNK));
        assert!(host.written[3].is_empty());
        let got: Vec<u8> = host.written[..3].concat()[12..].to_vec();
        assert_eq!(got, payload);
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn device_info_and_strings() {
        let (base, mut mtp) = setup("info");
        let r = run(&mut mtp, vec![cmd(GET_DEVICE_INFO, 1, &[])]);
        let d = &r[0].2;
        assert_eq!(le16(d, 0), 100);
        assert_eq!(read_str(d, 8).unwrap(), "microsoft.com: 1.0;");
        assert_eq!(response(&r).0, OK);
        let mut s = Vec::new();
        put_str(&mut s, "Ünïcødé ♪");
        assert_eq!(read_str(&s, 0).unwrap(), "Ünïcødé ♪");
        assert!(!valid_name("../x") && !valid_name(".bman-upload-a") && valid_name("a b.mp3"));
        fs::remove_dir_all(&base).unwrap();
    }
}
