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
