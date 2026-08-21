#include "SupportDir.hpp"

#include <QDir>
#include <QStandardPaths>

#include <cstdlib>

namespace dg {

QString SupportDir::base() {
    const char* override_ = std::getenv("DATAGREP_CONFIG_DIR");
    if (override_ != nullptr && override_[0] != '\0') {
        QString path = QString::fromLocal8Bit(override_);
        // A leading ~ arrives unexpanded when the var is set from a launcher
        // rather than a shell; the macOS app expands it, so this must too.
        if (path == QStringLiteral("~")) {
            path = QDir::homePath();
        } else if (path.startsWith(QStringLiteral("~/"))) {
            path = QDir::homePath() + path.mid(1);
        }
        return QDir::cleanPath(path);
    }
    return QStandardPaths::writableLocation(QStandardPaths::AppDataLocation);
}

QString SupportDir::ensured() {
    const QString dir = base();
    QDir().mkpath(dir);
    return dir;
}

}  // namespace dg
