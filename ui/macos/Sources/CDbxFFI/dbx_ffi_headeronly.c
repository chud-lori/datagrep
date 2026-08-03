/* CDbxFFI carries the frozen header only. SwiftPM requires at least one
 * source file per C target, so this TU exists purely to give it one.
 * The symbols themselves come from either CDbxStub (default) or the real
 * libdbx_ffi.a (DBX_FFI=real, see ../../Package.swift). */
#include "include/dbx_ffi.h"

const char *dbx_ffi_header_abi_tag(void);
const char *dbx_ffi_header_abi_tag(void) { return "dbx-ffi-abi-1"; }
