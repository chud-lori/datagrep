#include "QueryStatus.hpp"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonValue>

namespace dg {

QueryStatus QueryStatus::parse(const QString& json) {
    QueryStatus out;
    const QJsonDocument doc = QJsonDocument::fromJson(json.toUtf8());
    if (!doc.isObject()) {
        out.state = QueryState::Failed;
        out.error = QStringLiteral("unparseable status JSON");
        return out;
    }
    const QJsonObject o = doc.object();

    out.state = queryStateFromString(o.value(QStringLiteral("state")).toString());
    out.rowsLoaded =
        o.value(QStringLiteral("rows_loaded")).toVariant().toULongLong();
    out.elapsedMs =
        o.value(QStringLiteral("elapsed_ms")).toVariant().toULongLong();

    const QJsonValue affected = o.value(QStringLiteral("affected_rows"));
    if (!affected.isNull() && !affected.isUndefined()) {
        out.affectedRows = affected.toVariant().toULongLong();
    }

    const QJsonValue err = o.value(QStringLiteral("error"));
    if (err.isString()) {
        out.error = err.toString();
    }

    out.totalKnown = o.value(QStringLiteral("total_known")).toBool(false);

    const QJsonArray cols = o.value(QStringLiteral("columns")).toArray();
    out.columns.reserve(cols.size());
    for (const QJsonValue& c : cols) {
        const QJsonObject co = c.toObject();
        out.columns.push_back(ColumnSpec{
            co.value(QStringLiteral("name")).toString(),
            co.value(QStringLiteral("type")).toString(),
        });
    }

    out.editable = EditableResult::decode(o.value(QStringLiteral("editable")));

    const QJsonValue ro = o.value(QStringLiteral("read_only"));
    if (ro.isObject()) {
        const QJsonObject roo = ro.toObject();
        out.readOnlyEnforcement =
            roo.value(QStringLiteral("enforcement")).toString();
        out.readOnlyServerConfirmed =
            roo.value(QStringLiteral("server_confirmed")).toBool(false);
    }

    return out;
}

}  // namespace dg
