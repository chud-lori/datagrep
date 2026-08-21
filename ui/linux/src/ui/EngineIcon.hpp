// EngineIcon.hpp — the one place that knows what an engine looks like.

#ifndef DATAGREP_ENGINE_ICON_HPP
#define DATAGREP_ENGINE_ICON_HPP

#include <QColor>
#include <QIcon>
#include <QString>

namespace dg {

QString canonicalDriverId(const QString& id);

// "postgres" -> "PostgreSQL". Unknown ids are echoed back unchanged.
QString engineDisplayName(const QString& driverId);

QIcon engineIcon(const QString& driverId, const QColor& marker = QColor());

}  // namespace dg

#endif  // DATAGREP_ENGINE_ICON_HPP
