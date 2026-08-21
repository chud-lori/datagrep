// EngineIcon.hpp — the one place that knows what an engine looks like.
//
// Linux counterpart of DatagrepKit's EngineStyle: every surface that shows a
// connection — the sidebar list, the connection dialog's engine picker, tabs,
// history — asks here, so engine identity stays identical everywhere. There is
// deliberately no other driver-id folding or naming table in ui/linux.
//
// The artwork is the same SVG set the macOS target carries (single source,
// referenced by resources.qrc). Light/dark selection happens at PAINT time from
// the active application palette, so a runtime palette swap re-resolves the
// whole set with no notification wiring — the seam for any future explicit
// appearance setting is: apply it as the application palette.

#ifndef DATAGREP_ENGINE_ICON_HPP
#define DATAGREP_ENGINE_ICON_HPP

#include <QColor>
#include <QIcon>
#include <QString>

namespace dg {

// Folds driver-id spellings ("postgresql", "mariadb", "pg", ...) to the
// canonical id — the same table as EngineStyle.canonicalID on macOS. Public so
// no second folding table can grow elsewhere.
QString canonicalDriverId(const QString& id);

// "postgres" -> "PostgreSQL". Unknown ids are echoed back unchanged.
QString engineDisplayName(const QString& driverId);

// The engine's mark: brand artwork where it ships, a tinted drawn glyph where
// it does not (Elasticsearch, unknown drivers) — never blank. A valid `marker`
// adds the connection-colour bar down the leading edge; give the view a wider
// icon slot (e.g. 23x16) so the bar sits beside the mark, not over it.
QIcon engineIcon(const QString& driverId, const QColor& marker = QColor());

}  // namespace dg

#endif  // DATAGREP_ENGINE_ICON_HPP
