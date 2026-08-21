#include "SavedQueries.hpp"

#include "SupportDir.hpp"

#include <QDir>
#include <QFile>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QSaveFile>
#include <QSet>

#include <algorithm>

namespace {

// Key names match the macOS store's Codable output; absent keys mean "none",
// never empty strings.
QJsonObject recordToJson(const dg::SavedQueryRecord& r) {
    QJsonObject o;
    o.insert(QStringLiteral("id"), r.id);
    if (!r.name.isEmpty()) {
        o.insert(QStringLiteral("name"), r.name);
    }
    if (!r.connection.isEmpty()) {
        o.insert(QStringLiteral("connection"), r.connection);
    }
    o.insert(QStringLiteral("cursorLocation"), r.cursorLocation);
    o.insert(QStringLiteral("cursorLength"), r.cursorLength);
    o.insert(QStringLiteral("isDirty"), r.isDirty);
    return o;
}

dg::SavedQueryRecord recordFromJson(const QJsonObject& o) {
    dg::SavedQueryRecord r;
    r.id = o.value(QStringLiteral("id")).toString();
    r.name = o.value(QStringLiteral("name")).toString();
    r.connection = o.value(QStringLiteral("connection")).toString();
    r.cursorLocation = o.value(QStringLiteral("cursorLocation")).toInt(0);
    r.cursorLength = o.value(QStringLiteral("cursorLength")).toInt(0);
    r.isDirty = o.value(QStringLiteral("isDirty")).toBool(false);
    return r;
}

}  // namespace

namespace dg {

QString SavedQueryRecord::basename() const {
    if (!name.isEmpty()) {
        const QString s = SavedQueryStore::slug(name);
        if (!s.isEmpty()) {
            return s;
        }
    }
    return QStringLiteral("scratch-") + id;
}

}  // namespace dg

SavedQueryStore::SavedQueryStore(const QString& directory) : directory_(directory) {
    QDir().mkpath(directory_);
}

QString SavedQueryStore::defaultDirectory() {
    return dg::SupportDir::base() + QStringLiteral("/tabs");
}

QString SavedQueryStore::sqlPath(const dg::SavedQueryRecord& record) const {
    return directory_ + QStringLiteral("/") + record.basename() +
           QStringLiteral(".sql");
}

QString SavedQueryStore::sidecarPath(const dg::SavedQueryRecord& record) const {
    return directory_ + QStringLiteral("/") + record.basename() +
           QStringLiteral(".json");
}

// Atomic per file, so a crash mid-write leaves the previous version intact.
void SavedQueryStore::save(const dg::SavedQueryRecord& record, const QString& text) {
    QSaveFile sql(sqlPath(record));
    if (sql.open(QIODevice::WriteOnly | QIODevice::Text)) {
        sql.write(text.toUtf8());
        sql.commit();
    }
    QSaveFile sidecar(sidecarPath(record));
    if (sidecar.open(QIODevice::WriteOnly)) {
        sidecar.write(QJsonDocument(recordToJson(record))
                          .toJson(QJsonDocument::Indented));
        sidecar.commit();
    }
}

void SavedQueryStore::remove(const dg::SavedQueryRecord& record) {
    QFile::remove(sqlPath(record));
    QFile::remove(sidecarPath(record));
}

void SavedQueryStore::saveSession(const dg::EditorSession& session) {
    QJsonObject o;
    o.insert(QStringLiteral("order"), QJsonArray::fromStringList(session.order));
    if (!session.activeID.isEmpty()) {
        o.insert(QStringLiteral("activeID"), session.activeID);
    }
    if (!session.activeConnection.isEmpty()) {
        o.insert(QStringLiteral("activeConnection"), session.activeConnection);
    }
    QSaveFile f(directory_ + QStringLiteral("/session.json"));
    if (f.open(QIODevice::WriteOnly)) {
        f.write(QJsonDocument(o).toJson(QJsonDocument::Indented));
        f.commit();
    }
}

dg::EditorSession SavedQueryStore::loadSession() const {
    dg::EditorSession s;
    QFile f(directory_ + QStringLiteral("/session.json"));
    if (!f.open(QIODevice::ReadOnly)) {
        return s;
    }
    const QJsonObject o = QJsonDocument::fromJson(f.readAll()).object();
    const QJsonArray order = o.value(QStringLiteral("order")).toArray();
    for (const auto& v : order) {
        const QString id = v.toString();
        if (!id.isEmpty()) {
            s.order.append(id);
        }
    }
    s.activeID = o.value(QStringLiteral("activeID")).toString();
    s.activeConnection = o.value(QStringLiteral("activeConnection")).toString();
    return s;
}

QVector<dg::SavedQueryRecord> SavedQueryStore::allRecords() const {
    QVector<dg::SavedQueryRecord> out;
    QDir dir(directory_);
    const QStringList files =
        dir.entryList({QStringLiteral("*.json")}, QDir::Files, QDir::Name);
    for (const QString& name : files) {
        if (name == QStringLiteral("session.json")) {
            continue;
        }
        QFile f(dir.filePath(name));
        if (!f.open(QIODevice::ReadOnly)) {
            continue;
        }
        const QJsonDocument doc = QJsonDocument::fromJson(f.readAll());
        if (!doc.isObject()) {
            continue;
        }
        const dg::SavedQueryRecord r = recordFromJson(doc.object());
        if (!r.id.isEmpty()) {
            out.append(r);
        }
    }
    return out;
}

QString SavedQueryStore::text(const dg::SavedQueryRecord& record) const {
    QFile f(sqlPath(record));
    if (!f.open(QIODevice::ReadOnly | QIODevice::Text)) {
        return QString();
    }
    return QString::fromUtf8(f.readAll());
}

SavedQueryStore::Loaded SavedQueryStore::load() const {
    Loaded out;
    out.session = loadSession();

    QHash<QString, LoadedTab> byId;
    QStringList discovered;
    for (const dg::SavedQueryRecord& r : allRecords()) {
        // A record whose .sql has gone missing is dropped; a bare .sql with no
        // sidecar is ignored (no id, no connection, no caret — guessing).
        QFile f(sqlPath(r));
        if (!f.open(QIODevice::ReadOnly | QIODevice::Text)) {
            continue;
        }
        byId.insert(r.id, LoadedTab{r, QString::fromUtf8(f.readAll())});
        discovered.append(r.id);
    }

    QSet<QString> seen;
    for (const QString& id : out.session.order) {
        const auto it = byId.constFind(id);
        if (it != byId.constEnd() && !seen.contains(id)) {
            seen.insert(id);
            out.tabs.append(*it);
        }
    }
    // Scratch tabs the session forgot are reopened anyway — unsaved work has
    // nowhere else to live. Named ones stay closed; the saved list holds them.
    std::sort(discovered.begin(), discovered.end());
    for (const QString& id : discovered) {
        const auto it = byId.constFind(id);
        if (it != byId.constEnd() && it->record.isScratch() && !seen.contains(id)) {
            seen.insert(id);
            out.tabs.append(*it);
        }
    }

    if (!out.session.activeID.isEmpty() && !seen.contains(out.session.activeID)) {
        out.session.activeID.clear();
    }
    return out;
}

// Filesystem-safe lower-kebab, matching the macOS slug so a synced tabs
// directory stays one set of files.
QString SavedQueryStore::slug(const QString& name) {
    QString out;
    bool lastWasDash = false;
    for (const QChar ch : name.toLower()) {
        if (ch.isLetterOrNumber()) {
            out.append(ch);
            lastWasDash = false;
        } else if (!lastWasDash && !out.isEmpty()) {
            out.append(QLatin1Char('-'));
            lastWasDash = true;
        }
    }
    while (out.endsWith(QLatin1Char('-'))) {
        out.chop(1);
    }
    return out.left(64);
}
