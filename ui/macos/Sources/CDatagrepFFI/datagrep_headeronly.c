/* SwiftPM needs one source file per C target; the symbols come from CDatagrepStub or libdatagrep_ffi.a. */
#include "include/datagrep.h"

const char *datagrep_header_abi_tag(void);
const char *datagrep_header_abi_tag(void) { return "datagrep-ffi-abi-1"; }
