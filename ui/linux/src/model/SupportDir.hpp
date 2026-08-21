// SupportDir.hpp — where datagrep keeps everything it owns: profiles.sqlite,
// tabs/ and history/.
//
// Linux counterpart of DatagrepKit.SupportDirectory. `DATAGREP_CONFIG_DIR`
// overrides the whole tree — the CLI and the macOS app both honour it, and
// without it there is no way to run this UI against anything but the machine's
// real connections. Unset, the Qt-native per-user data directory is used.

#ifndef DATAGREP_SUPPORT_DIR_HPP
#define DATAGREP_SUPPORT_DIR_HPP

#include <QString>

namespace dg {

class SupportDir {
public:
    // The base directory. Does not touch the filesystem.
    static QString base();

    // The base directory, created if it is not there yet.
    static QString ensured();
};

}  // namespace dg

#endif  // DATAGREP_SUPPORT_DIR_HPP
