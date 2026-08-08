/* CDatagrepFFI carries the frozen header only. SwiftPM requires at least one
 * source file per C target, so this TU exists purely to give it one.
 * The symbols themselves come from either CDatagrepStub (default) or the real
 * libdatagrep_ffi.a (DATAGREP_FFI=real, see ../../Package.swift). */
#include "include/datagrep.h"

const char *datagrep_header_abi_tag(void);
const char *datagrep_header_abi_tag(void) { return "datagrep-ffi-abi-1"; }
