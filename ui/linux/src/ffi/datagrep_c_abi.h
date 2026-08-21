// datagrep_c_abi.h — the one place the frozen C ABI header is included.

// datagrep.h ships no extern "C" guard; always include this wrapper, never datagrep.h.
#ifndef DATAGREP_C_ABI_H
#define DATAGREP_C_ABI_H

extern "C" {
#include "datagrep.h"  // resolved via target_include_directories -> crates/datagrep-ffi/include
}

#endif  // DATAGREP_C_ABI_H
