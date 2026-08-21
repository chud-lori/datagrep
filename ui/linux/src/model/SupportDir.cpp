#include "SupportDir.hpp"

#include <QDir>
#include <QStandardPaths>

#include <cstdlib>

namespace dg {

QString SupportDir::base() {
    const char* override_ = std::getenv("DATAGREP_CONFIG_DIR");
    if (override_ != nullptr && override_[0] != '\0') {
        QString path = QString::fromLocal8Bit(override_);
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
