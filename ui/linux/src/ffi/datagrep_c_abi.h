// datagrep_c_abi.h — the one place the frozen C ABI header is included.
//
// crates/datagrep-ffi/include/datagrep.h is the canonical, FROZEN interface and
// is shared verbatim with the macOS build. It declares its functions as plain C
// with NO `extern "C"` guard (it targets a C compiler and Swift's C importer,
// neither of which mangles names). A C++ translation unit that included it
// directly would give every symbol C++ linkage and then fail to resolve them
// against libdatagrep_ffi.a, whose exports are C-linkage.
//
// The engine/crate must not change to suit this UI, so instead of editing the
// header we wrap the include in `extern "C"` here. Every other file in the
// Linux UI includes THIS header (or DatagrepFfi.hpp), never datagrep.h directly.

#ifndef DATAGREP_C_ABI_H
#define DATAGREP_C_ABI_H

extern "C" {
#include "datagrep.h"  // resolved via target_include_directories -> crates/datagrep-ffi/include
}

#endif  // DATAGREP_C_ABI_H
