// ConnectionSafety.hpp — the safety facts about one connection.

#ifndef DATAGREP_CONNECTION_SAFETY_HPP
#define DATAGREP_CONNECTION_SAFETY_HPP

#include <QColor>
#include <QString>

#include <optional>

namespace dg {

// The safety-relevant slice of one profile, as parsed from profiles_list JSON.
struct ConnectionSafety {
    QString name;
    QString color;          // one of the marker palette names; empty = unmarked
    bool readOnly = false;
    bool confirmWrites = false;

    bool isMarked() const { return !color.isEmpty(); }
};

std::optional<QColor> connectionColor(const QString& name);

// Fat-finger guardrail, not security; read-only enforcement lives in the engine.
bool isWriteStatement(const QString& sql);

QString statementVerb(const QString& sql);

}  // namespace dg

#endif  // DATAGREP_CONNECTION_SAFETY_HPP
