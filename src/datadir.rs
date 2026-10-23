//! Persistent data folder `%ProgramData%\mini-oled-dock` and handle-based file I/O inside it.
//!
//! WHY: the dock runs elevated. Its data used to live in `%LOCALAPPDATA%\Dock`, which every
//! medium-integrity process of the same user can write: planting a junction, symlink or hard link
//! there turns the elevated writes, renames and deletes into an arbitrary file write/delete (a
//! classic local privilege escalation). The new root is created by the elevated process with an
//! explicit PROTECTED DACL (nothing inherited from `ProgramData`, where Users may create folders):
//! SYSTEM and Administrators full control, Users read and execute only. Non-admins cannot create,
//! rename or delete entries in it, so nothing can be planted.
//!
//! LAYOUT:
//!  * `<root>\config.json`: the only file the interactive user may change. Its own protected DACL
//!    adds one ACE for the token user: read + write data (no DELETE, no WRITE_DAC). Every
//!    programmatic write creates a temp file with that same DACL and renames it over the old one
//!    (`write_atomic`), so the ACE is never lost and the replace is atomic.
//!  * `<root>\config.json.bak`, `dock.log`, `dock.log.1`, `taskbar-state`, `cache\`: admin-only
//!    write (they inherit the root DACL).
//!  * `<root>\icons\`: extra ACEs let the token user add files (and change the files inside). Since
//!    the user can create entries there, custom icons are opened WITHOUT following reparse points,
//!    must be single-link regular files whose final path is exactly `<icons>\<name>`, and are decoded
//!    from memory (`read_icon`). The elevated process never writes into `icons\`.
//!
//! MODES: `init()` runs once, in the processes that need persistent data (dock, watchdog,
//! `--install`/`--uninstall`, `--restore`). Elevated: creates, verifies and repairs the folders.
//! Not elevated: verifies only and never writes (an unelevated create would itself squat the root,
//! since ProgramData lets Users create folders). If the root is unusable every path is `None`:
//! config defaults, no cache, log only to the debugger output. Before `init()` every path is
//! `None` too, so `--preview` and `--list-apps` never create or touch the root.
//!
//! SQUATTING: a medium process could pre-create the root before the first install. A root whose
//! owner is not Administrators/SYSTEM is hostile: it is renamed away through the very handle that
//! was inspected (no path race, no follow of reparse points) and recreated; if that fails it is
//! refused with a message telling the user to delete it from an elevated prompt. An admin-owned root
//! whose DACL lets non-admins write is treated the same way (moved aside as `.old-...`, recreated):
//! anything inside may have been planted while it was loose, so nothing in it is reused except its
//! `config.json`, read back through the checked path (`read_previous_config`).
//!
//! THREADING: everything is thread-safe (`OnceLock` state, no COM).

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use windows::core::{BOOL, GUID, HSTRING, PWSTR};
use windows::Win32::Foundation::{LocalFree, ERROR_ALREADY_EXISTS, GENERIC_WRITE, HANDLE, HLOCAL};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    AclSizeInformation, GetAce, GetAclInformation, GetKernelObjectSecurity, GetSecurityDescriptorDacl,
    GetSecurityDescriptorOwner, GetTokenInformation, SetKernelObjectSecurity, TokenUser, ACE_HEADER, ACL,
    ACL_SIZE_INFORMATION, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    CreateDirectoryW, CreateFileW, FileRenameInfo, GetFileInformationByHandle, GetFinalPathNameByHandleW, MoveFileExW,
    SetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, CREATE_NEW, DELETE, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES,
    FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GETFINALPATHNAMEBYHANDLE_FLAGS,
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, OPEN_EXISTING, READ_CONTROL, SYNCHRONIZE, VOLUME_NAME_DOS,
    WRITE_DAC,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcessToken};
use windows::Win32::UI::Shell::{FOLDERID_LocalAppData, FOLDERID_ProgramData, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

/// Folder name under `%ProgramData%`.
pub const ROOT_NAME: &str = "mini-oled-dock";
const CACHE_NAME: &str = "cache";
const ICONS_NAME: &str = "icons";

/// Size caps of the files read into memory.
pub const CONFIG_MAX_BYTES: u64 = 1024 * 1024;
pub const ICON_MAX_BYTES: u64 = 4 * 1024 * 1024;
pub const CACHE_MAX_BYTES: u64 = 1024 * 1024;
pub const STATE_MAX_BYTES: u64 = 64;

// ------------------------------------------------------------------ access masks (pure)

/// SIDs trusted as owner/writer of the data folders.
const SID_SYSTEM: &str = "S-1-5-18";
const SID_ADMINISTRATORS: &str = "S-1-5-32-544";

/// Read + execute (list/traverse for folders): `FILE_GENERIC_READ | FILE_GENERIC_EXECUTE` (0x1200a9).
const MASK_READ_EXECUTE: u32 = FILE_GENERIC_READ.0 | FILE_GENERIC_EXECUTE.0;
/// The user's ACE on `config.json`: read + write data, NOT delete, NOT WRITE_DAC (0x12019f).
const MASK_CONFIG_USER: u32 = FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0;
/// The user's ACE on the `icons` folder itself: list (1), add file (2), read EA (8), traverse (0x20),
/// read attributes (0x80), READ_CONTROL, SYNCHRONIZE. No add-subfolder, no delete-child, no WRITE_DAC.
const MASK_ICONS_DIR_USER: u32 = 0x1 | 0x2 | 0x8 | 0x20 | 0x80 | READ_CONTROL.0 | SYNCHRONIZE.0;
/// Inherit-only ACE for the files inside `icons`: read, write and delete them (so the user can replace
/// or remove the PNGs they dropped). No WRITE_DAC beyond what the owner of a file implicitly has.
const MASK_ICONS_FILE_USER: u32 = FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0 | DELETE.0;

/// Rights that let a trustee change a folder/file or its entries: write data / add file (0x2),
/// append / add subfolder (0x4), write EA (0x10), delete child (0x40), write attributes (0x100),
/// DELETE, WRITE_DAC, WRITE_OWNER, GENERIC_WRITE, GENERIC_ALL.
const MASK_WRITE_LIKE: u32 =
    0x2 | 0x4 | 0x10 | 0x40 | 0x100 | 0x0001_0000 | 0x0004_0000 | 0x0008_0000 | 0x4000_0000 | 0x1000_0000;

/// SDDL of the root: owner Administrators, protected DACL, inheritable to files and subfolders.
fn root_sddl() -> String {
    format!("O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x{MASK_READ_EXECUTE:x};;;BU)")
}

/// SDDL of `config.json`: like the root (not inheritable: it is a file) plus read + write data for `user`.
fn config_sddl(user: &str) -> Option<String> {
    is_sid_string(user).then(|| {
        format!("O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x{MASK_READ_EXECUTE:x};;;BU)(A;;0x{MASK_CONFIG_USER:x};;;{user})")
    })
}

/// SDDL of `icons\`: the root ACEs plus "add files" on the folder and an inherit-only ACE that gives
/// `user` read/write/delete on the files created inside.
fn icons_sddl(user: &str) -> Option<String> {
    is_sid_string(user).then(|| {
        format!(
            "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x{MASK_READ_EXECUTE:x};;;BU)\
             (A;;0x{MASK_ICONS_DIR_USER:x};;;{user})(A;OIIO;0x{MASK_ICONS_FILE_USER:x};;;{user})"
        )
    })
}

/// true if `s` looks like a string SID (`S-1-<n>[-<n>...]`, non-empty decimal parts only): it is
/// spliced into SDDL, so nothing else may get through. Pure.
fn is_sid_string(s: &str) -> bool {
    s.len() <= 200
        && s.strip_prefix("S-1-")
            .is_some_and(|rest| rest.split('-').all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())))
}

/// true if `sid` is SYSTEM or the Administrators group. Pure.
fn is_admin_sid(sid: &str) -> bool {
    sid.eq_ignore_ascii_case(SID_SYSTEM) || sid.eq_ignore_ascii_case(SID_ADMINISTRATORS)
}

/// One ACE of a DACL, reduced to what the checks need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AceFacts {
    /// Deny ACE (any deny type): it can only remove rights, so it never makes a folder unsafe.
    pub deny: bool,
    /// Trustee as string SID; `None` for ACE types whose layout is not parsed (treated as untrusted).
    pub sid: Option<String>,
    pub mask: u32,
}

/// true if the DACL grants no write-like right to anyone but SYSTEM/Administrators. A NULL DACL
/// (`None`) grants everything to everyone. Inherit-only ACEs count too (they reach the children). Pure.
pub(crate) fn dacl_admin_only(dacl: Option<&[AceFacts]>) -> bool {
    let Some(aces) = dacl else { return false };
    aces.iter().all(|a| a.deny || a.mask & MASK_WRITE_LIKE == 0 || a.sid.as_deref().is_some_and(is_admin_sid))
}

/// What was found at a data folder path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DirFacts {
    pub is_dir: bool,
    pub is_reparse: bool,
    /// `Some(true)` = owned by SYSTEM/Administrators; `None` = owner unreadable.
    pub owner_trusted: Option<bool>,
    pub admin_only: bool,
}

/// Decision about an existing data folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Safe as is.
    Use,
    /// Admin-owned real folder with a DACL that lets non-admins write. The root is then moved aside
    /// and recreated (its children may have been planted); a subfolder of a verified root only gets
    /// the protected DACL re-applied.
    RepairDacl,
    /// Not owned by an admin (squatting): never used; may be renamed away.
    Hostile,
    /// Admin-owned but not usable (reparse point, not a folder, or a bad DACL we may not fix).
    Refuse,
}

/// Pure decision for a folder that already exists. `can_repair` = elevated (owner-admin can fix the DACL).
pub(crate) fn classify(f: &DirFacts, can_repair: bool) -> Verdict {
    if f.owner_trusted != Some(true) {
        return Verdict::Hostile;
    }
    if f.is_reparse || !f.is_dir {
        return Verdict::Refuse;
    }
    match (f.admin_only, can_repair) {
        (true, _) => Verdict::Use,
        (false, true) => Verdict::RepairDacl,
        (false, false) => Verdict::Refuse,
    }
}

/// Checks the attributes of an opened file: refuses reparse points, folders and (if `single_link`)
/// files with more than one hard link. Pure.
pub(crate) fn check_regular(attrs: u32, links: u32, single_link: bool) -> Result<(), &'static str> {
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        Err("it is a reparse point (symlink or junction)")
    } else if attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
        Err("it is a folder")
    } else if single_link && links != 1 {
        Err("it has more than one hard link")
    } else {
        Ok(())
    }
}

/// true if `final_path` (from `GetFinalPathNameByHandleW` on the opened file) is exactly
/// `<dir_final>\<name>` (case-insensitive). Pure.
pub(crate) fn final_path_matches(final_path: &str, dir_final: &str, name: &str) -> bool {
    let expected = format!("{}\\{}", dir_final.trim_end_matches('\\'), name);
    final_path.to_lowercase() == expected.to_lowercase()
}

// ------------------------------------------------------------------ state

struct Root {
    path: PathBuf,
    /// Elevated and verified: the process may write (config, cache, log, state).
    writable: bool,
    /// `cache\` verified (admin-only, real folder).
    cache_ok: bool,
    /// Final path of `icons\` (for `read_icon`); `None` = custom icons disabled.
    icons_final: Option<String>,
    /// Token user as string SID (ACE on config.json).
    user_sid: Option<String>,
    /// Final path of an admin-owned root moved aside this run because its DACL was loose.
    previous_final: Option<String>,
}

static ROOT: OnceLock<Option<Root>> = OnceLock::new();

fn root_ref() -> Option<&'static Root> {
    ROOT.get().and_then(Option::as_ref)
}

/// Sets up the data root (create/verify/repair if elevated, verify only otherwise). Returns the
/// diagnostic lines to log once the logger runs (they are also sent to the debugger output now).
/// Only the first call does anything.
pub fn init() -> Vec<String> {
    if ROOT.get().is_some() {
        return Vec::new();
    }
    let mut notes = Vec::new();
    let r = init_root(&mut notes);
    let _ = ROOT.set(r);
    for n in &notes {
        debug_out(n);
    }
    notes
}

/// true if this process may write in the root (elevated, root verified).
pub fn writable() -> bool {
    root_ref().is_some_and(|r| r.writable)
}

/// `<root>\config.json`.
pub fn config_path() -> Option<PathBuf> {
    root_ref().map(|r| r.path.join("config.json"))
}

/// `<root>\cache` (icon PNG cache), only if verified.
pub fn cache_dir() -> Option<PathBuf> {
    root_ref().filter(|r| r.cache_ok).map(|r| r.path.join(CACHE_NAME))
}

/// `<root>\icons` (the user's custom PNGs), only if verified.
pub fn icons_dir() -> Option<PathBuf> {
    root_ref().filter(|r| r.icons_final.is_some()).map(|r| r.path.join(ICONS_NAME))
}

/// `<root>\dock.log`, only in a writable root.
pub fn log_path() -> Option<PathBuf> {
    root_ref().filter(|r| r.writable).map(|r| r.path.join("dock.log"))
}

/// `<root>\taskbar-state`.
pub fn state_path() -> Option<PathBuf> {
    root_ref().map(|r| r.path.join("taskbar-state"))
}

/// Old data folder `%LOCALAPPDATA%\Dock` (medium-writable: only read, for the one-time migration).
/// Works without `init()`.
pub fn legacy_dir() -> Option<PathBuf> {
    known_folder(&FOLDERID_LocalAppData).map(|p| p.join("Dock"))
}

/// Sends a line to the debugger output (DebugView): the fallback when there is no log file.
pub fn debug_out(msg: &str) {
    let s = HSTRING::from(format!("mini-oled-dock: {msg}\n"));
    // SAFETY: `s` is a NUL-terminated wide string that lives for the call.
    unsafe { OutputDebugStringW(&s) };
}

// ------------------------------------------------------------------ init

fn init_root(notes: &mut Vec<String>) -> Option<Root> {
    let elevated = crate::shell_thread::is_elevated();
    let Some(pd) = known_folder(&FOLDERID_ProgramData) else {
        notes.push("data: %ProgramData% not found: running without persistent data".into());
        return None;
    };
    let path = pd.join(ROOT_NAME);
    let user_sid = token_user_sid();
    if user_sid.is_none() {
        notes.push("data: the user SID could not be read: config.json and icons\\ stay admin-only".into());
    }
    let mut previous_final = None;
    if elevated {
        if !ensure_root(&path, notes, &mut previous_final) {
            return None;
        }
    } else {
        match open_dir(&path, false).and_then(|d| dir_facts(&d)) {
            Ok(f) if classify(&f, false) == Verdict::Use => {}
            Ok(f) => {
                notes.push(format!(
                    "data: {} is not trustworthy ({:?}): running without persistent data",
                    path.display(),
                    classify(&f, false)
                ));
                return None;
            }
            Err(e) => {
                notes.push(format!(
                    "data: {} not usable without elevation ({e}): running without persistent data",
                    path.display()
                ));
                return None;
            }
        }
    }
    let cache_ok = ensure_subdir(&path.join(CACHE_NAME), None, elevated, notes).is_some();
    let icons_sd = user_sid.as_deref().and_then(icons_sddl);
    let icons_final = ensure_subdir(&path.join(ICONS_NAME), icons_sd.as_deref(), elevated, notes);
    Some(Root { path, writable: elevated, cache_ok, icons_final, user_sid, previous_final })
}

/// Elevated: creates the root with the protected DACL or validates the existing one. A hostile
/// (squatted) root, or an admin-owned one with a loose DACL, is renamed away once through the
/// inspected handle, then recreated. For the loose admin-owned case `previous` receives the final
/// path of the moved folder (its config.json may be migrated).
fn ensure_root(path: &Path, notes: &mut Vec<String>, previous: &mut Option<String>) -> bool {
    let shown = path.display();
    for attempt in 0..2 {
        match create_dir(path, Some(&root_sddl())) {
            Ok(()) => notes.push(format!("data: created {shown} (admin-only)")),
            Err(e) if e.code() == ERROR_ALREADY_EXISTS.to_hresult() => {}
            Err(e) => {
                notes.push(format!("data: cannot create {shown} ({e}): running without persistent data"));
                return false;
            }
        }
        let dir = match open_dir(path, true) {
            Ok(d) => d,
            Err(e) => {
                notes.push(format!("data: cannot open {shown} ({e}): running without persistent data"));
                return false;
            }
        };
        let facts = match dir_facts(&dir) {
            Ok(f) => f,
            Err(e) => {
                notes.push(format!("data: cannot inspect {shown} ({e}): running without persistent data"));
                return false;
            }
        };
        match classify(&facts, true) {
            Verdict::Use => return true,
            Verdict::RepairDacl => {
                // Admin-owned but non-admins could write: anything inside may have been planted while
                // the DACL was loose, and fixing the root alone would not fix the children. Nothing
                // pre-existing is trusted: the whole folder is moved aside and recreated; only its
                // config.json may later be read back through the checked path (`read_previous_config`).
                if attempt == 0 {
                    match rename_away(&dir, path, "old") {
                        Ok(new_name) => {
                            *previous = final_path(handle_of(&dir.handle));
                            notes.push(format!(
                                "data: {shown} let non-admins write: moved to '{new_name}' (delete it once \
                                 you checked it), creating a fresh one"
                            ));
                            continue;
                        }
                        Err(e) => notes.push(format!("data: moving the loosely secured {shown} away failed: {e}")),
                    }
                }
                notes.push(format!(
                    "data: {shown} lets non-admins write: NOT used, running without persistent data. Delete it \
                     from an elevated prompt (rmdir /s /q \"{shown}\") and restart the dock."
                ));
                return false;
            }
            Verdict::Hostile => {
                if attempt == 0 {
                    match rename_away(&dir, path, "untrusted") {
                        Ok(new_name) => {
                            notes.push(format!(
                                "data: {shown} was not created by an administrator (possible squatting): \
                                 renamed to '{new_name}' (delete it), creating a fresh one"
                            ));
                            continue;
                        }
                        Err(e) => notes.push(format!("data: renaming the untrusted {shown} away failed: {e}")),
                    }
                }
                notes.push(format!(
                    "data: {shown} is not owned by Administrators/SYSTEM (possible squatting): NOT used, running \
                     without persistent data. Delete it from an elevated prompt (rmdir /s /q \"{shown}\") and \
                     restart the dock."
                ));
                return false;
            }
            Verdict::Refuse => {
                notes.push(format!(
                    "data: {shown} is a reparse point or not a folder: NOT used, running without persistent \
                     data. Delete it from an elevated prompt (rmdir \"{shown}\") and restart the dock."
                ));
                return false;
            }
        }
    }
    false
}

/// Creates/validates a subfolder of the (already validated) root. `sddl` = explicit DACL to apply
/// (always re-applied when elevated, so the user ACE follows the current user); `None` = inherit the
/// root DACL and require admin-only. Returns the folder's final path, `None` = not usable.
fn ensure_subdir(path: &Path, sddl: Option<&str>, elevated: bool, notes: &mut Vec<String>) -> Option<String> {
    let shown = path.display();
    if elevated {
        match create_dir(path, sddl) {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_ALREADY_EXISTS.to_hresult() => {}
            Err(e) => {
                notes.push(format!("data: cannot create {shown}: {e}"));
                return None;
            }
        }
    }
    let dir = open_dir(path, elevated).map_err(|e| notes.push(format!("data: cannot open {shown}: {e}"))).ok()?;
    let mut facts = dir_facts(&dir).map_err(|e| notes.push(format!("data: cannot inspect {shown}: {e}"))).ok()?;
    if sddl.is_some() {
        // The user ACE is expected here; the DACL is re-applied below anyway.
        facts.admin_only = true;
    }
    match classify(&facts, elevated) {
        Verdict::Use => {}
        Verdict::RepairDacl => match set_dacl(&dir, &root_sddl()) {
            Ok(()) => notes.push(format!("data: {shown} let non-admins write: protected DACL re-applied")),
            Err(e) => {
                notes.push(format!("data: {shown} lets non-admins write and could not be fixed ({e}): not used"));
                return None;
            }
        },
        v => {
            notes.push(format!("data: {shown} is not usable ({v:?}): not used"));
            return None;
        }
    }
    if let (true, Some(s)) = (elevated, sddl) {
        if let Err(e) = set_dacl(&dir, s) {
            notes.push(format!("data: applying the DACL of {shown} failed ({e}): you may not be able to add files"));
        }
    }
    final_path(handle_of(&dir.handle))
}

// ------------------------------------------------------------------ file I/O

/// Who may change a file written by `write_atomic`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileAcl {
    /// Inherit the folder's DACL (admin-only inside the root).
    Inherit,
    /// `config.json`: explicit protected DACL with the token user's read + write-data ACE.
    UserEditable,
}

/// Opens `path` for reading WITHOUT following reparse points and checks it is a regular file
/// (single hard link if `single_link`). Errors keep `NotFound` for a missing file.
fn open_checked(path: &Path, single_link: bool) -> io::Result<File> {
    let f = OpenOptions::new()
        .read(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)?;
    let info = file_info(&f)?;
    check_regular(info.dwFileAttributes, info.nNumberOfLinks, single_link)
        .map_err(|e| io::Error::new(io::ErrorKind::PermissionDenied, e))?;
    Ok(f)
}

fn read_capped(f: File, cap: u64) -> io::Result<Vec<u8>> {
    let mut v = Vec::new();
    f.take(cap + 1).read_to_end(&mut v)?;
    if v.len() as u64 > cap {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("larger than {cap} bytes")));
    }
    Ok(v)
}

/// Reads a whole file (at most `cap` bytes) through a handle opened without following reparse
/// points; refuses reparse points, folders and (if `single_link`) hard-linked files.
pub fn read_file(path: &Path, cap: u64, single_link: bool) -> io::Result<Vec<u8>> {
    read_capped(open_checked(path, single_link)?, cap)
}

/// Reads the custom icon `<root>\icons\<name>` (the user can create entries there): plain name only,
/// opened without following reparse points, single-link regular file, final path exactly
/// `<icons>\<name>`, at most `ICON_MAX_BYTES`. The bytes are decoded by the caller from memory.
pub fn read_icon(name: &str) -> io::Result<Vec<u8>> {
    if !crate::icons::is_plain_icon_name(name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a plain file name"));
    }
    let r = root_ref().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no data folder"))?;
    let dir_final =
        r.icons_final.as_deref().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no icons folder"))?;
    let f = open_checked(&r.path.join(ICONS_NAME).join(name), true)?;
    let fin = final_path(handle_of(&f)).ok_or_else(|| io::Error::other("final path unavailable"))?;
    if !final_path_matches(&fin, dir_final, name) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("resolves to {fin}, outside the icons folder"),
        ));
    }
    read_capped(f, ICON_MAX_BYTES)
}

/// Reads the legacy `%LOCALAPPDATA%\Dock\config.json` for the one-time migration. `Ok(None)` = absent.
/// The folder is medium-writable, so: no reparse points, a single hard link, final path exactly
/// `<legacy dir>\config.json` (no redirected parent), owner = the token user or Administrators/SYSTEM
/// (older versions wrote it from the elevated process, so it is admin-owned; the user could edit it
/// anyway, and its content is parsed as untrusted), at most `CONFIG_MAX_BYTES`.
pub fn read_legacy_config() -> Result<Option<Vec<u8>>, String> {
    let Some(dir) = legacy_dir() else { return Ok(None) };
    let path = dir.join("config.json");
    let f = match open_checked(&path, true) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let fin = final_path(handle_of(&f)).unwrap_or_default();
    // Expected location in final-path form (`\\?\C:\...`), built from the known-folder path itself, NOT
    // from resolving the directory (which would follow a planted junction).
    let dir_s = dir.to_string_lossy();
    let expected_dir = if dir_s.starts_with(r"\\?\") { dir_s.into_owned() } else { format!(r"\\?\{dir_s}") };
    if !final_path_matches(&fin, &expected_dir, "config.json") {
        return Err(format!("{}: resolves to {fin}, skipped", path.display()));
    }
    let owner = read_security(handle_of(&f)).map_err(|e| format!("{}: {e}", path.display()))?.owner;
    let user = token_user_sid();
    let trusted = owner.is_some() && (owner == user || owner.as_deref().is_some_and(is_admin_sid));
    if !trusted {
        return Err(format!("{}: not owned by the current user or Administrators/SYSTEM", path.display()));
    }
    read_capped(f, CONFIG_MAX_BYTES).map(Some).map_err(|e| format!("{}: {e}", path.display()))
}

/// Reads `config.json` from a data folder moved aside this run because its DACL was loose
/// (`ensure_root`). `Ok(None)` = no such folder or no config. Anything may have been planted in
/// it, so: no reparse points, a single hard link, final path exactly `<moved folder>\config.json`
/// (no redirected parent), owner Administrators/SYSTEM (a file the dock itself wrote; a planted one
/// is skipped), at most `CONFIG_MAX_BYTES`.
pub fn read_previous_config() -> Result<Option<Vec<u8>>, String> {
    let Some(prev) = root_ref().and_then(|r| r.previous_final.as_deref()) else { return Ok(None) };
    let path = PathBuf::from(prev).join("config.json");
    let f = match open_checked(&path, true) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let fin = final_path(handle_of(&f)).unwrap_or_default();
    if !final_path_matches(&fin, prev, "config.json") {
        return Err(format!("{}: resolves to {fin}, skipped", path.display()));
    }
    let owner = read_security(handle_of(&f)).map_err(|e| format!("{}: {e}", path.display()))?.owner;
    if !owner.as_deref().is_some_and(is_admin_sid) {
        return Err(format!("{}: not owned by Administrators/SYSTEM, skipped", path.display()));
    }
    read_capped(f, CONFIG_MAX_BYTES).map(Some).map_err(|e| format!("{}: {e}", path.display()))
}

/// Atomic write: `<path>.tmp` is created NEW (never through an existing name or link) with the
/// requested DACL, written, flushed, then renamed over `path` (a rename replaces the directory
/// entry, it never follows a link at the destination). Chosen over an in-place truncate+write
/// because the file is never seen half-written and the DACL travels with the new file.
pub fn write_atomic(path: &Path, bytes: &[u8], acl: FileAcl) -> Result<(), String> {
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    // A stale temp (crash): deleting a name removes the link itself, never a link target.
    match std::fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("removing {}: {e}", tmp.display())),
    }
    let sddl = match acl {
        FileAcl::Inherit => None,
        FileAcl::UserEditable => root_ref().and_then(|r| r.user_sid.as_deref()).and_then(config_sddl),
    };
    let sd = sddl.as_deref().map(SecDesc::from_sddl).transpose()?;
    let sa = sd.as_ref().map(|sd| SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0 .0,
        bInheritHandle: BOOL(0),
    });
    let wide = HSTRING::from(tmp.as_os_str());
    // SAFETY: `wide` is NUL-terminated; `sa` (and the descriptor it points to, owned by `sd`) outlive
    // the call. CREATE_NEW + FILE_FLAG_OPEN_REPARSE_POINT never opens an existing name or link.
    let h = unsafe {
        CreateFileW(
            &wide,
            GENERIC_WRITE.0,
            windows::Win32::Storage::FileSystem::FILE_SHARE_MODE(0),
            sa.as_ref().map(|s| s as *const SECURITY_ATTRIBUTES),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(|e| format!("creating {}: {e}", tmp.display()))?;
    // SAFETY: `h` is a valid file handle just returned by CreateFileW and owned by nobody else.
    let mut f = File::from(unsafe { OwnedHandle::from_raw_handle(h.0) });
    let res = f.write_all(bytes).and_then(|()| f.sync_all());
    drop(f);
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("writing {}: {e}", tmp.display()));
    }
    let (src, dst) = (HSTRING::from(tmp.as_os_str()), HSTRING::from(path.as_os_str()));
    // SAFETY: both strings are NUL-terminated and live for the call.
    if let Err(e) = unsafe { MoveFileExW(&src, &dst, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) } {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("replacing {}: {e}", path.display()));
    }
    Ok(())
}

/// Opens `path` for appending (creating it if missing) without following reparse points; if the
/// name is a link or a hard-linked file it is deleted (the name only) and created fresh once.
pub fn open_append(path: &Path) -> Option<File> {
    for _ in 0..2 {
        let f = OpenOptions::new()
            .append(true)
            .create(true)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
            .ok()?;
        let ok = file_info(&f).is_ok_and(|i| check_regular(i.dwFileAttributes, i.nNumberOfLinks, true).is_ok());
        if ok {
            return Some(f);
        }
        drop(f);
        // Deleting a name never touches the target of a link.
        std::fs::remove_file(path).ok()?;
    }
    None
}

// ------------------------------------------------------------------ Win32 helpers

fn handle_of<T: AsRawHandle>(h: &T) -> HANDLE {
    HANDLE(h.as_raw_handle())
}

fn file_info<T: AsRawHandle>(h: &T) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: valid open handle; `info` is a writable struct of the declared type.
    unsafe { GetFileInformationByHandle(handle_of(h), &mut info) }.map_err(io::Error::other)?;
    Ok(info)
}

/// Final normalized DOS path of an open handle (`\\?\C:\...`).
fn final_path(h: HANDLE) -> Option<String> {
    let mut buf = vec![0u16; 512];
    for _ in 0..2 {
        // SAFETY: `h` is a valid handle; `buf` is writable for its whole length.
        let n = unsafe {
            GetFinalPathNameByHandleW(
                h,
                &mut buf,
                GETFINALPATHNAMEBYHANDLE_FLAGS(FILE_NAME_NORMALIZED.0 | VOLUME_NAME_DOS.0),
            )
        } as usize;
        if n == 0 {
            return None;
        }
        if n < buf.len() {
            return Some(String::from_utf16_lossy(&buf[..n]));
        }
        buf = vec![0u16; n + 1];
    }
    None
}

/// Opened folder (or whatever is at the path: reparse points are not followed).
struct OpenDir {
    handle: OwnedHandle,
    /// The handle has READ_CONTROL (owner/DACL readable).
    can_read_sd: bool,
}

fn open_raw(path: &Path, access: u32) -> windows::core::Result<OwnedHandle> {
    let wide = HSTRING::from(path.as_os_str());
    // SAFETY: `wide` is NUL-terminated and lives for the call; BACKUP_SEMANTICS opens folders and
    // OPEN_REPARSE_POINT opens a junction/symlink itself instead of its target.
    let h = unsafe {
        CreateFileW(
            &wide,
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }?;
    // SAFETY: fresh valid handle, ownership transferred to the OwnedHandle.
    Ok(unsafe { OwnedHandle::from_raw_handle(h.0) })
}

/// Opens a data folder. `for_write` (elevated): tries to also get WRITE_DAC + DELETE (repair,
/// rename away), falling back to read-only access, and finally to DELETE alone (owner unreadable).
fn open_dir(path: &Path, for_write: bool) -> Result<OpenDir, String> {
    let ra = FILE_READ_ATTRIBUTES.0;
    let rc = READ_CONTROL.0;
    let attempts: &[u32] = if for_write {
        &[rc | WRITE_DAC.0 | DELETE.0 | ra, rc | WRITE_DAC.0 | ra, rc | ra, DELETE.0 | ra]
    } else {
        &[rc | ra]
    };
    let mut last = String::new();
    for &access in attempts {
        match open_raw(path, access) {
            Ok(handle) => return Ok(OpenDir { handle, can_read_sd: access & rc != 0 }),
            Err(e) => last = e.to_string(),
        }
    }
    Err(last)
}

fn dir_facts(d: &OpenDir) -> Result<DirFacts, String> {
    let info = file_info(&d.handle).map_err(|e| e.to_string())?;
    let attrs = info.dwFileAttributes;
    let (owner_trusted, admin_only) = if d.can_read_sd {
        let sec = read_security(handle_of(&d.handle))?;
        (Some(sec.owner.as_deref().is_some_and(is_admin_sid)), dacl_admin_only(sec.dacl.as_deref()))
    } else {
        (None, false)
    };
    Ok(DirFacts {
        is_dir: attrs & FILE_ATTRIBUTE_DIRECTORY.0 != 0,
        is_reparse: attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0,
        owner_trusted,
        admin_only,
    })
}

struct SecInfo {
    owner: Option<String>,
    /// `None` = NULL DACL.
    dacl: Option<Vec<AceFacts>>,
}

/// Owner and DACL of an open handle (needs READ_CONTROL).
fn read_security(h: HANDLE) -> Result<SecInfo, String> {
    let what = (OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION).0;
    let mut needed = 0u32;
    // SAFETY: size query with no buffer; `needed` is writable. The expected "buffer too small" error is ignored.
    let _ = unsafe { GetKernelObjectSecurity(h, what, None, 0, &mut needed) };
    if needed == 0 {
        return Err("GetKernelObjectSecurity: no size".into());
    }
    // u64 storage: the self-relative descriptor needs pointer alignment.
    let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
    let sd = PSECURITY_DESCRIPTOR(buf.as_mut_ptr().cast());
    // SAFETY: `buf` holds at least `needed` writable bytes; `h` is a valid handle.
    unsafe { GetKernelObjectSecurity(h, what, Some(sd), needed, &mut needed) }
        .map_err(|e| format!("GetKernelObjectSecurity: {e}"))?;
    let mut owner = PSID::default();
    let mut defaulted = BOOL(0);
    // SAFETY: `sd` is the valid self-relative descriptor in `buf`; out-params are writable. The returned
    // SID points into `buf`, which outlives its use below.
    unsafe { GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted) }.map_err(|e| format!("owner: {e}"))?;
    let owner = sid_string(owner);
    let mut present = BOOL(0);
    let mut acl: *mut ACL = std::ptr::null_mut();
    // SAFETY: as above; the ACL pointer points into `buf`.
    unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut acl, &mut defaulted) }
        .map_err(|e| format!("dacl: {e}"))?;
    let dacl = if present.as_bool() && !acl.is_null() { Some(read_aces(acl)?) } else { None };
    drop(buf);
    Ok(SecInfo { owner, dacl })
}

/// ACE type codes (winnt.h) of the deny family: plain, object, callback, callback-object.
const DENY_ACE_TYPES: [u8; 4] = [1, 6, 10, 12];
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;

fn read_aces(acl: *const ACL) -> Result<Vec<AceFacts>, String> {
    let mut info = ACL_SIZE_INFORMATION::default();
    // SAFETY: `acl` is a valid ACL (from a valid descriptor); `info` is writable and sized as declared.
    unsafe {
        GetAclInformation(
            acl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast::<c_void>(),
            std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    }
    .map_err(|e| format!("GetAclInformation: {e}"))?;
    let mut out = Vec::with_capacity(info.AceCount as usize);
    for i in 0..info.AceCount {
        let mut p: *mut c_void = std::ptr::null_mut();
        // SAFETY: `i` < AceCount; `p` is writable.
        unsafe { GetAce(acl, i, &mut p) }.map_err(|e| format!("GetAce: {e}"))?;
        if p.is_null() {
            return Err("GetAce: null".into());
        }
        // SAFETY: every ACE starts with ACE_HEADER followed by the 32-bit access mask (all ACE types);
        // unaligned reads avoid any alignment assumption.
        let (hdr, mask) = unsafe {
            (
                std::ptr::read_unaligned(p.cast::<ACE_HEADER>()),
                std::ptr::read_unaligned(p.cast::<u8>().add(4).cast::<u32>()),
            )
        };
        let deny = DENY_ACE_TYPES.contains(&hdr.AceType);
        let sid = if hdr.AceType == ACCESS_ALLOWED_ACE_TYPE || hdr.AceType == DENY_ACE_TYPES[0] {
            // SAFETY: ACCESS_ALLOWED_ACE / ACCESS_DENIED_ACE: header (4) + mask (4) + SID.
            sid_string(PSID(unsafe { p.cast::<u8>().add(8) }.cast()))
        } else {
            None
        };
        out.push(AceFacts { deny, sid, mask });
    }
    Ok(out)
}

/// String form of a SID (`S-1-...`).
fn sid_string(sid: PSID) -> Option<String> {
    if sid.0.is_null() {
        return None;
    }
    let mut s = PWSTR::null();
    // SAFETY: `sid` points to a valid SID; `s` receives a LocalAlloc'd string freed below.
    unsafe { ConvertSidToStringSidW(sid, &mut s) }.ok()?;
    // SAFETY: `s` is a valid NUL-terminated string returned by the call above.
    let out = unsafe { s.to_string() }.ok();
    // SAFETY: allocated by ConvertSidToStringSidW with LocalAlloc; not used afterwards.
    unsafe { LocalFree(Some(HLOCAL(s.0.cast()))) };
    out
}

/// Token user of this process as string SID (the account whose desktop the dock serves).
fn token_user_sid() -> Option<String> {
    let mut token = HANDLE::default();
    // SAFETY: pseudo-handle of the current process; `token` is writable.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.ok()?;
    // SAFETY: fresh valid token handle, closed when `_owned` drops.
    let _owned = unsafe { OwnedHandle::from_raw_handle(token.0) };
    let mut len = 0u32;
    // SAFETY: size query; `len` is writable. The expected "buffer too small" error is ignored.
    let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut len) };
    if (len as usize) < std::mem::size_of::<TOKEN_USER>() {
        return None;
    }
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `len` writable, pointer-aligned bytes.
    unsafe { GetTokenInformation(token, TokenUser, Some(buf.as_mut_ptr().cast()), len, &mut len) }.ok()?;
    // SAFETY: the buffer now starts with a TOKEN_USER whose SID points inside `buf` (alive here).
    let tu = unsafe { std::ptr::read(buf.as_ptr().cast::<TOKEN_USER>()) };
    sid_string(tu.User.Sid)
}

fn known_folder(id: &GUID) -> Option<PathBuf> {
    // SAFETY: `id` is a valid KNOWNFOLDERID; no token = current user. The PWSTR is freed below.
    let p = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None) }.ok()?;
    // SAFETY: `p` is a NUL-terminated string returned by the call above.
    let s = unsafe { p.to_string() }.ok();
    // SAFETY: allocated with CoTaskMemAlloc by SHGetKnownFolderPath; not used afterwards.
    unsafe { CoTaskMemFree(Some(p.0 as *const c_void)) };
    s.filter(|s| !s.is_empty()).map(PathBuf::from)
}

/// Self-relative security descriptor built from SDDL (LocalFree'd on drop).
struct SecDesc(PSECURITY_DESCRIPTOR);

impl SecDesc {
    fn from_sddl(sddl: &str) -> Result<SecDesc, String> {
        let wide = HSTRING::from(sddl);
        let mut sd = PSECURITY_DESCRIPTOR::default();
        // SAFETY: `wide` is NUL-terminated; `sd` receives a LocalAlloc'd descriptor owned by the result.
        unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(&wide, SDDL_REVISION_1, &mut sd, None) }
            .map_err(|e| format!("SDDL: {e}"))?;
        Ok(SecDesc(sd))
    }
}

impl Drop for SecDesc {
    fn drop(&mut self) {
        // SAFETY: allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW; freed once.
        unsafe { LocalFree(Some(HLOCAL(self.0 .0))) };
    }
}

/// `CreateDirectoryW` with the descriptor of `sddl` (or inherited ACEs if `None`). Atomic: the folder
/// never exists with a weaker DACL.
fn create_dir(path: &Path, sddl: Option<&str>) -> windows::core::Result<()> {
    let sd = match sddl.map(SecDesc::from_sddl).transpose() {
        Ok(sd) => sd,
        Err(e) => return Err(windows::core::Error::new(windows::core::HRESULT(0x8007_000Du32 as i32), e)),
    };
    let sa = sd.as_ref().map(|sd| SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.0 .0,
        bInheritHandle: BOOL(0),
    });
    let wide = HSTRING::from(path.as_os_str());
    // SAFETY: `wide` is NUL-terminated; `sa` and the descriptor it points to outlive the call.
    unsafe { CreateDirectoryW(&wide, sa.as_ref().map(|s| s as *const SECURITY_ATTRIBUTES)) }
}

/// Replaces the DACL of an open folder (handle-based: `SetKernelObjectSecurity` never propagates to
/// children, so it cannot be steered through a planted link inside).
fn set_dacl(d: &OpenDir, sddl: &str) -> Result<(), String> {
    let sd = SecDesc::from_sddl(sddl)?;
    let h = handle_of(&d.handle);
    // SAFETY: valid handle (opened with WRITE_DAC when possible, otherwise the call fails) and a
    // valid descriptor that lives for the call.
    let r =
        unsafe { SetKernelObjectSecurity(h, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION, sd.0) };
    match r {
        Ok(()) => Ok(()),
        // The descriptor itself carries SE_DACL_PROTECTED ("D:P"): retry without the extra flag.
        // SAFETY: as above.
        Err(_) => unsafe { SetKernelObjectSecurity(h, DACL_SECURITY_INFORMATION, sd.0) }.map_err(|e| e.to_string()),
    }
}

/// Renames the inspected object (folder or reparse point) to `<ROOT_NAME>.<tag>-<pid>-<n>` in the
/// same parent as `path` (where it was opened), through its own handle: exactly what was inspected is
/// moved, nothing is followed, nothing is written into it. Fails if the target name exists.
/// The target must be a full path: kernel32 resolves a bare name against the current directory.
fn rename_away(d: &OpenDir, path: &Path, tag: &str) -> Result<String, String> {
    let parent = path.parent().ok_or_else(|| "no parent folder".to_string())?;
    // SAFETY: no preconditions.
    let pid = unsafe { GetCurrentProcessId() };
    let nanos =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|t| t.subsec_nanos()).unwrap_or(0);
    let name = format!("{ROOT_NAME}.{tag}-{pid}-{nanos}");
    let target = parent.join(&name);
    let wide: Vec<u16> = target.as_os_str().encode_wide().collect();
    let name_bytes = wide.len() * 2;
    let total = std::mem::size_of::<FILE_RENAME_INFO>() + name_bytes;
    // Zeroed, u64-aligned buffer: ReplaceIfExists = false, RootDirectory = NULL.
    let mut buf = vec![0u64; total.div_ceil(8)];
    let info = buf.as_mut_ptr().cast::<FILE_RENAME_INFO>();
    // SAFETY: `buf` is large enough for the header plus the name (the trailing NUL comes from the
    // zeroed `FileName[1]` slot); writes stay inside it.
    unsafe {
        (*info).FileNameLength = name_bytes as u32;
        std::ptr::copy_nonoverlapping(
            wide.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            wide.len(),
        );
    }
    // SAFETY: handle opened with DELETE (otherwise the call fails); buffer valid for `total` bytes.
    unsafe { SetFileInformationByHandle(handle_of(&d.handle), FileRenameInfo, info.cast_const().cast(), total as u32) }
        .map_err(|e| e.to_string())?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allow(sid: &str, mask: u32) -> AceFacts {
        AceFacts { deny: false, sid: Some(sid.into()), mask }
    }

    #[test]
    fn masks_match_the_documented_values() {
        assert_eq!(MASK_READ_EXECUTE, 0x1200a9);
        assert_eq!(MASK_CONFIG_USER, 0x12019f);
        // The config ACE must not allow delete, WRITE_DAC or WRITE_OWNER.
        assert_eq!(MASK_CONFIG_USER & (DELETE.0 | WRITE_DAC.0 | 0x0008_0000), 0);
        // The icons folder ACE: add file but no add-subfolder (0x4), delete-child (0x40) or WRITE_DAC.
        assert_ne!(MASK_ICONS_DIR_USER & 0x2, 0);
        assert_eq!(MASK_ICONS_DIR_USER & (0x4 | 0x40 | DELETE.0 | WRITE_DAC.0), 0);
        assert_eq!(MASK_ICONS_FILE_USER & WRITE_DAC.0, 0);
        assert_eq!(MASK_READ_EXECUTE & MASK_WRITE_LIKE, 0);
    }

    #[test]
    fn sddl_strings() {
        assert_eq!(root_sddl(), "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)");
        let sid = "S-1-5-21-1-2-3-1001";
        assert_eq!(
            config_sddl(sid).as_deref(),
            Some("O:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;0x1200a9;;;BU)(A;;0x12019f;;;S-1-5-21-1-2-3-1001)")
        );
        let icons = icons_sddl(sid).unwrap_or_default();
        assert!(icons.starts_with("O:BAD:P(A;OICI;FA;;;SY)"));
        assert!(icons.contains("(A;;0x1200ab;;;S-1-5-21-1-2-3-1001)"));
        assert!(icons.contains("(A;OIIO;0x13019f;;;S-1-5-21-1-2-3-1001)"));
        // SDDL injection through the SID is impossible.
        assert!(config_sddl("S-1-5)(A;;FA;;;WD").is_none());
        assert!(icons_sddl("WD").is_none());
        assert!(config_sddl("").is_none());
    }

    #[test]
    fn sid_strings() {
        assert!(is_sid_string("S-1-5-18"));
        assert!(!is_sid_string("S-1-"));
        assert!(is_admin_sid("S-1-5-32-544") && is_admin_sid("s-1-5-18"));
        assert!(!is_admin_sid("S-1-5-32-545") && !is_admin_sid("S-1-5-21-1-2-3-1001"));
    }

    #[test]
    fn dacl_checks() {
        let ours = [allow(SID_SYSTEM, 0x1f01ff), allow(SID_ADMINISTRATORS, 0x1f01ff), allow("S-1-5-32-545", 0x1200a9)];
        assert!(dacl_admin_only(Some(&ours)));
        // NULL DACL = full access for everyone.
        assert!(!dacl_admin_only(None));
        // ProgramData's default: Users may create folders / append (0x4) and write data (0x2).
        assert!(!dacl_admin_only(Some(&[allow("S-1-5-32-545", 0x1200a9 | 0x6)])));
        // CREATOR OWNER full control (inherit-only) is still a writer.
        assert!(!dacl_admin_only(Some(&[allow("S-1-3-0", 0x1000_0000)])));
        for m in [0x2, 0x4, 0x10, 0x40, 0x100, 0x1_0000, 0x4_0000, 0x8_0000, 0x4000_0000, 0x1000_0000] {
            assert!(!dacl_admin_only(Some(&[allow("S-1-1-0", m)])), "{m:#x}");
        }
        // Deny ACEs never weaken; unknown ACE types with write rights do.
        assert!(dacl_admin_only(Some(&[AceFacts { deny: true, sid: None, mask: 0x1f01ff }])));
        assert!(!dacl_admin_only(Some(&[AceFacts { deny: false, sid: None, mask: 0x2 }])));
        assert!(dacl_admin_only(Some(&[AceFacts { deny: false, sid: None, mask: 0x1200a9 }])));
    }

    #[test]
    fn classify_decisions() {
        let good = DirFacts { is_dir: true, is_reparse: false, owner_trusted: Some(true), admin_only: true };
        assert_eq!(classify(&good, true), Verdict::Use);
        assert_eq!(classify(&good, false), Verdict::Use);
        let loose = DirFacts { admin_only: false, ..good };
        assert_eq!(classify(&loose, true), Verdict::RepairDacl);
        assert_eq!(classify(&loose, false), Verdict::Refuse);
        // Squatting: owned by a user, or owner unreadable, whatever else it is.
        for owner in [Some(false), None] {
            assert_eq!(classify(&DirFacts { owner_trusted: owner, ..good }, true), Verdict::Hostile);
            let junction = DirFacts { owner_trusted: owner, is_reparse: true, ..good };
            assert_eq!(classify(&junction, true), Verdict::Hostile);
        }
        // Admin-owned junction or file: refused, never repaired.
        assert_eq!(classify(&DirFacts { is_reparse: true, ..good }, true), Verdict::Refuse);
        assert_eq!(classify(&DirFacts { is_dir: false, ..good }, true), Verdict::Refuse);
    }

    #[test]
    fn regular_file_checks() {
        let normal = FILE_ATTRIBUTE_NORMAL.0;
        assert!(check_regular(normal, 1, true).is_ok());
        assert!(check_regular(normal, 2, false).is_ok());
        assert!(check_regular(normal, 2, true).is_err());
        assert!(check_regular(normal | FILE_ATTRIBUTE_REPARSE_POINT.0, 1, true).is_err());
        assert!(check_regular(FILE_ATTRIBUTE_DIRECTORY.0, 1, false).is_err());
    }

    #[test]
    fn final_path_checks() {
        let dir = r"\\?\C:\ProgramData\mini-oled-dock\icons";
        assert!(final_path_matches(r"\\?\C:\ProgramData\mini-oled-dock\icons\a.png", dir, "a.png"));
        assert!(final_path_matches(r"\\?\c:\programdata\MINI-OLED-DOCK\Icons\A.PNG", dir, "a.png"));
        assert!(final_path_matches(r"\\?\C:\ProgramData\mini-oled-dock\icons\a.png", &format!("{dir}\\"), "a.png"));
        // Redirected (hard link elsewhere, symlinked parent, short-name trick): refused.
        assert!(!final_path_matches(r"\\?\C:\Windows\System32\a.png", dir, "a.png"));
        assert!(!final_path_matches(r"\\?\C:\ProgramData\mini-oled-dock\icons\sub\a.png", dir, "a.png"));
        assert!(!final_path_matches(r"\\?\C:\ProgramData\mini-oled-dock\icons\b.png", dir, "a.png"));
    }

    #[test]
    fn nothing_before_init() {
        // Unit tests never call `init`: no path is handed out, nothing is created.
        assert!(config_path().is_none() && log_path().is_none() && cache_dir().is_none() && !writable());
        assert!(read_icon("a.png").is_err());
    }

    #[test]
    fn atomic_write_and_checked_read_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dock_datadir_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("f.txt");
        assert!(write_atomic(&p, b"one", FileAcl::Inherit).is_ok());
        assert!(write_atomic(&p, b"two", FileAcl::Inherit).is_ok());
        assert_eq!(read_file(&p, 16, true).ok().as_deref(), Some(&b"two"[..]));
        assert!(read_file(&p, 2, true).is_err());
        assert_eq!(
            read_file(&dir.join("missing"), 16, true).map_err(|e| e.kind()).err(),
            Some(io::ErrorKind::NotFound)
        );
        // A folder is not a regular file.
        assert!(read_file(&dir, 16, false).is_err());
        // A hard-linked file is refused when a single link is required, and `open_append` replaces
        // the linked name with a fresh file instead of appending through it.
        let link = dir.join("link.txt");
        assert!(std::fs::hard_link(&p, &link).is_ok());
        assert!(read_file(&p, 16, true).is_err());
        assert!(read_file(&p, 16, false).is_ok());
        let appended = open_append(&link).map(|mut f| f.write_all(b"x").is_ok());
        assert_eq!(appended, Some(true));
        assert_eq!(read_file(&p, 16, true).ok().as_deref(), Some(&b"two"[..]));
        assert_eq!(read_file(&link, 16, true).ok().as_deref(), Some(&b"x"[..]));
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn sddl_strings_parse() {
        let sid = "S-1-5-21-1-2-3-1001";
        for s in [Some(root_sddl()), config_sddl(sid), icons_sddl(sid)] {
            let s = s.unwrap_or_default();
            assert!(SecDesc::from_sddl(&s).is_ok(), "{s}");
        }
    }

    /// Real NTFS round trip, unelevated: a folder created with an explicit DACL is read back ACE by
    /// ACE, re-secured through its handle and renamed away through its handle.
    #[test]
    fn real_folder_inspection_repair_and_rename() {
        let Some(me) = token_user_sid() else { panic!("no token user") };
        let base = std::env::temp_dir().join(format!("dock_datadir_sec_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        let d = base.join("probe");
        let full = format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{me})");
        assert!(create_dir(&d, Some(&full)).is_ok());
        let od = open_dir(&d, true).unwrap_or_else(|e| panic!("open: {e}"));
        let f = dir_facts(&od).unwrap_or_else(|e| panic!("facts: {e}"));
        assert!(f.is_dir && !f.is_reparse && !f.admin_only);
        // Unelevated, the owner is the user: hostile from the elevated dock's point of view.
        assert_eq!(f.owner_trusted, Some(crate::shell_thread::is_elevated()));
        let aces = read_security(handle_of(&od.handle)).ok().and_then(|s| s.dacl).unwrap_or_default();
        let sids: Vec<_> = aces.iter().filter_map(|a| a.sid.clone()).collect();
        assert_eq!(sids, vec![SID_SYSTEM.to_string(), SID_ADMINISTRATORS.to_string(), me.clone()]);
        assert!(aces.iter().all(|a| !a.deny && a.mask == 0x1f01ff));
        // Read-only for the user: admin-only.
        assert!(set_dacl(&od, &format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;{me})")).is_ok());
        assert!(dir_facts(&od).is_ok_and(|f| f.admin_only));
        // Final path of a file inside matches `<dir final>\<name>`.
        let file = d.join("a.png");
        assert!(std::fs::write(&file, b"x").is_err() || std::fs::remove_file(&file).is_ok());
        assert!(set_dacl(&od, &full).is_ok());
        assert!(std::fs::write(&file, b"x").is_ok());
        let dir_final = final_path(handle_of(&od.handle)).unwrap_or_default();
        let fin = open_checked(&file, true).ok().and_then(|f| final_path(handle_of(&f))).unwrap_or_default();
        assert!(final_path_matches(&fin, &dir_final, "a.png"), "{fin} vs {dir_final}");
        let _ = std::fs::remove_file(&file);
        // Rename through the inspected handle (simple name = same parent).
        let new_name = rename_away(&od, &d, "untrusted").unwrap_or_else(|e| panic!("rename: {e}"));
        drop(od);
        assert!(new_name.starts_with("mini-oled-dock.untrusted-"));
        assert!(base.join(&new_name).is_dir() && std::fs::symlink_metadata(&d).is_err());
        let _ = std::fs::remove_dir(base.join(&new_name));
        let _ = std::fs::remove_dir(&base);
    }
}
