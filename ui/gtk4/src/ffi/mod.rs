use std::ffi::{c_char, c_void, CStr, CString};
use std::fmt;

use datagrep_ffi::{
    datagrep_catalog_children_json, datagrep_catalog_describe_json, datagrep_core_free,
    datagrep_core_new, datagrep_profiles_add, datagrep_profiles_list_json, datagrep_query_cancel,
    datagrep_query_free, datagrep_query_on_progress, datagrep_query_rows, datagrep_query_run,
    datagrep_query_status_json, datagrep_rows_cell, datagrep_rows_cell_detail_json,
    datagrep_rows_cell_kind, datagrep_rows_columns, datagrep_rows_count,
    datagrep_rows_envelope_json, datagrep_rows_free, datagrep_rows_pending, datagrep_string_free,
    DatagrepCore, DatagrepQuery, DatagrepRows,
};
// Not re-exported at the crate root, unlike the rest of the ABI surface.
use datagrep_ffi::profiles::{
    datagrep_connection_info_json, datagrep_connection_test_json, datagrep_profiles_add_json,
    datagrep_profiles_get_json, datagrep_profiles_update,
};

use crate::model::WindowMeta;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn owned_string_from_ffi(p: *mut c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let owned = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
    unsafe { datagrep_string_free(p) };
    Some(owned)
}

fn error_from_ffi(err: *mut c_char) -> Error {
    Error(
        owned_string_from_ffi(err)
            .unwrap_or_else(|| "the engine failed without a message".to_owned()),
    )
}

fn nul_terminated(s: &str) -> Result<CString, Error> {
    CString::new(s).map_err(|_| Error(format!("`{}` contains a NUL byte", s.escape_debug())))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellKind {
    Value,
    Null,
    Absent,
    Nested,
    Pending,
}

impl CellKind {
    fn from_raw(raw: u8) -> Self {
        match raw {
            1 => CellKind::Null,
            2 => CellKind::Absent,
            3 => CellKind::Nested,
            _ => CellKind::Value,
        }
    }
}

pub struct Core {
    raw: *mut DatagrepCore,
}

// The ABI serialises internally; macOS and Qt already call it from arbitrary threads.
unsafe impl Send for Core {}
unsafe impl Sync for Core {}

impl Core {
    pub fn open(profiles_db_path: &str) -> Result<Self, Error> {
        let path = nul_terminated(profiles_db_path)?;
        let mut err: *mut c_char = std::ptr::null_mut();
        let raw = unsafe { datagrep_core_new(path.as_ptr(), &mut err) };
        if raw.is_null() {
            return Err(error_from_ffi(err));
        }
        Ok(Self { raw })
    }

    pub fn profiles_add(&self, name: &str, url: &str) -> Result<(), Error> {
        let (name, url) = (nul_terminated(name)?, nul_terminated(url)?);
        let mut err: *mut c_char = std::ptr::null_mut();
        let added =
            unsafe { datagrep_profiles_add(self.raw, name.as_ptr(), url.as_ptr(), &mut err) };
        if added {
            Ok(())
        } else {
            Err(error_from_ffi(err))
        }
    }

    pub fn profiles_list_json(&self) -> Result<String, Error> {
        let mut err: *mut c_char = std::ptr::null_mut();
        let raw = unsafe { datagrep_profiles_list_json(self.raw, &mut err) };
        owned_string_from_ffi(raw).ok_or_else(|| error_from_ffi(err))
    }

    /// One level of children under `path_json`; never recurses, never crawls.
    pub fn catalog_children_json(&self, profile: &str, path_json: &str) -> Result<String, Error> {
        let (profile, path) = (nul_terminated(profile)?, nul_terminated(path_json)?);
        let mut err: *mut c_char = std::ptr::null_mut();
        let raw = unsafe {
            datagrep_catalog_children_json(self.raw, profile.as_ptr(), path.as_ptr(), &mut err)
        };
        owned_string_from_ffi(raw).ok_or_else(|| error_from_ffi(err))
    }

    /// Columns, indexes and stats for one object — fetched only when this is called.
    pub fn catalog_describe_json(&self, profile: &str, path_json: &str) -> Result<String, Error> {
        let (profile, path) = (nul_terminated(profile)?, nul_terminated(path_json)?);
        let mut err: *mut c_char = std::ptr::null_mut();
        let raw = unsafe {
            datagrep_catalog_describe_json(self.raw, profile.as_ptr(), path.as_ptr(), &mut err)
        };
        owned_string_from_ffi(raw).ok_or_else(|| error_from_ffi(err))
    }

    pub fn profile_json(&self, name: &str) -> Result<String, Error> {
        let name = nul_terminated(name)?;
        let mut err: *mut c_char = std::ptr::null_mut();
        let raw = unsafe { datagrep_profiles_get_json(self.raw, name.as_ptr(), &mut err) };
        owned_string_from_ffi(raw).ok_or_else(|| error_from_ffi(err))
    }

    pub fn add_profile_json(&self, name: &str, url: &str, options_json: &str) -> Result<(), Error> {
        let (name, url) = (nul_terminated(name)?, nul_terminated(url)?);
        let options = nul_terminated(options_json)?;
        let mut err: *mut c_char = std::ptr::null_mut();
        let added = unsafe {
            datagrep_profiles_add_json(
                self.raw,
                name.as_ptr(),
                url.as_ptr(),
                options.as_ptr(),
                &mut err,
            )
        };
        if added {
            Ok(())
        } else {
            Err(error_from_ffi(err))
        }
    }

    pub fn update_profile(&self, name: &str, patch_json: &str) -> Result<(), Error> {
        let (name, patch) = (nul_terminated(name)?, nul_terminated(patch_json)?);
        let mut err: *mut c_char = std::ptr::null_mut();
        let updated =
            unsafe { datagrep_profiles_update(self.raw, name.as_ptr(), patch.as_ptr(), &mut err) };
        if updated {
            Ok(())
        } else {
            Err(error_from_ffi(err))
        }
    }

    pub fn connection_info_json(&self, name: &str) -> Result<String, Error> {
        let name = nul_terminated(name)?;
        let mut err: *mut c_char = std::ptr::null_mut();
        let raw = unsafe { datagrep_connection_info_json(self.raw, name.as_ptr(), &mut err) };
        owned_string_from_ffi(raw).ok_or_else(|| error_from_ffi(err))
    }

    /// Opens one connection and closes it again; nothing is saved by testing.
    pub fn test_connection_json(&self, name: &str, url: &str) -> Result<String, Error> {
        let (name, url) = (nul_terminated(name)?, nul_terminated(url)?);
        let mut err: *mut c_char = std::ptr::null_mut();
        let raw = unsafe {
            datagrep_connection_test_json(self.raw, name.as_ptr(), url.as_ptr(), &mut err)
        };
        owned_string_from_ffi(raw).ok_or_else(|| error_from_ffi(err))
    }

    pub fn query(&self, profile: &str, sql: &str) -> Result<Query, Error> {
        let (profile, sql) = (nul_terminated(profile)?, nul_terminated(sql)?);
        let mut err: *mut c_char = std::ptr::null_mut();
        let raw = unsafe { datagrep_query_run(self.raw, profile.as_ptr(), sql.as_ptr(), &mut err) };
        if raw.is_null() {
            return Err(error_from_ffi(err));
        }
        Ok(Query {
            raw,
            progress: None,
        })
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        unsafe { datagrep_core_free(self.raw) };
    }
}

type ProgressFn = Box<dyn Fn() + Send + Sync>;

pub struct Query {
    raw: *mut DatagrepQuery,
    progress: Option<Box<ProgressFn>>,
}

impl Query {
    /// `handler` runs on a tokio worker thread; hopping to the main context is its own job.
    pub fn on_progress(&mut self, handler: impl Fn() + Send + Sync + 'static) {
        let next: Box<ProgressFn> = Box::new(Box::new(handler));
        let ctx = (&*next as *const ProgressFn as *mut ProgressFn).cast::<c_void>();
        // Register before dropping the old closure: the ABI swaps (cb, ctx) under
        // the lock it fires from.
        unsafe { datagrep_query_on_progress(self.raw, Some(trampoline), ctx) };
        self.progress = Some(next);
    }

    pub fn status_json(&self) -> Result<String, Error> {
        let mut err: *mut c_char = std::ptr::null_mut();
        let raw = unsafe { datagrep_query_status_json(self.raw, &mut err) };
        owned_string_from_ffi(raw).ok_or_else(|| error_from_ffi(err))
    }

    pub fn cancel(&self) -> Option<String> {
        let mut outcome: *mut c_char = std::ptr::null_mut();
        unsafe { datagrep_query_cancel(self.raw, &mut outcome) };
        owned_string_from_ffi(outcome)
    }

    pub fn rows(&self, offset: u64, len: u64) -> Result<RowWindow, Error> {
        let mut err: *mut c_char = std::ptr::null_mut();
        let raw = unsafe { datagrep_query_rows(self.raw, offset, len, &mut err) };
        if raw.is_null() {
            return Err(error_from_ffi(err));
        }
        Ok(RowWindow::adopt(raw, offset))
    }
}

impl Drop for Query {
    fn drop(&mut self) {
        // Detaches the callback, so no thread can still be reading `progress`.
        unsafe { datagrep_query_free(self.raw) };
        self.progress = None;
    }
}

extern "C" fn trampoline(ctx: *mut c_void) {
    if ctx.is_null() {
        return;
    }
    let handler = unsafe { &*ctx.cast::<ProgressFn>() };
    handler();
}

pub struct RowWindow {
    raw: *mut DatagrepRows,
    offset: u64,
    count: u64,
    columns: u32,
    pending: bool,
}

impl RowWindow {
    fn adopt(raw: *mut DatagrepRows, offset: u64) -> Self {
        unsafe {
            Self {
                offset,
                count: datagrep_rows_count(raw),
                columns: datagrep_rows_columns(raw),
                pending: datagrep_rows_pending(raw),
                raw,
            }
        }
    }

    pub fn columns(&self) -> u32 {
        self.columns
    }

    pub fn pending(&self) -> bool {
        self.pending
    }

    // The ABI indexes row * cols + col, so an out-of-range column would read
    // another row's cell rather than fail.
    fn local(&self, row: u64, col: u32) -> Option<u64> {
        (self.contains(row) && col < self.columns).then(|| row - self.offset)
    }

    pub fn cell(&self, row: u64, col: u32) -> Option<&str> {
        let local = self.local(row, col)?;
        let mut len: usize = 0;
        let p = unsafe { datagrep_rows_cell(self.raw, local, col, &mut len) };
        if p.is_null() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(p.cast::<u8>(), len) };
        std::str::from_utf8(bytes).ok()
    }

    pub fn kind(&self, row: u64, col: u32) -> Option<CellKind> {
        let local = self.local(row, col)?;
        Some(CellKind::from_raw(unsafe {
            datagrep_rows_cell_kind(self.raw, local, col)
        }))
    }

    pub fn cell_detail_json(&self, row: u64, col: u32) -> Option<String> {
        let local = self.local(row, col)?;
        owned_string_from_ffi(unsafe { datagrep_rows_cell_detail_json(self.raw, local, col) })
    }

    pub fn envelope_json(&self, row: u64) -> Option<String> {
        if !self.contains(row) {
            return None;
        }
        owned_string_from_ffi(unsafe { datagrep_rows_envelope_json(self.raw, row - self.offset) })
    }
}

impl WindowMeta for RowWindow {
    fn offset(&self) -> u64 {
        self.offset
    }
    fn count(&self) -> u64 {
        self.count
    }
}

impl Drop for RowWindow {
    fn drop(&mut self) {
        unsafe { datagrep_rows_free(self.raw) };
    }
}
