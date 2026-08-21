#include "QueryHistory.hpp"

#include <QDir>
#include <QFile>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLocale>
#include <QSaveFile>
#include <QStandardPaths>
#include <QUuid>

#include <algorithm>
#include <climits>
#include <utility>

namespace dg {

QString outcomeKey(QueryOutcome o) {
    switch (o) {
        case QueryOutcome::Ok: return QStringLiteral("ok");
        case QueryOutcome::Error: return QStringLiteral("error");
        case QueryOutcome::Cancelled: return QStringLiteral("cancelled");
    }
    return QStringLiteral("ok");
}

QString outcomeLabel(QueryOutcome o) {
    switch (o) {
        case QueryOutcome::Ok: return QStringLiteral("ok");
        case QueryOutcome::Error: return QStringLiteral("failed");
        case QueryOutcome::Cancelled: return QStringLiteral("cancelled");
    }
    return QStringLiteral("ok");
}

std::optional<QueryOutcome> outcomeFromKey(const QString& key) {
    if (key == QStringLiteral("ok")) return QueryOutcome::Ok;
    if (key == QStringLiteral("error")) return QueryOutcome::Error;
    if (key == QStringLiteral("cancelled")) return QueryOutcome::Cancelled;
    return std::nullopt;
}

QString QueryHistoryEntry::oneLine() const {
    QString flat = sql;
    flat.replace(QLatin1Char('\t'), QLatin1Char(' '));
    QStringList parts;
    const QStringList lines = flat.split(QLatin1Char('\n'), Qt::SkipEmptyParts);
    for (const QString& line : lines) {
        const QString t = line.trimmed();
        if (!t.isEmpty()) {
            parts << t;
        }
    }
    const QString joined = parts.join(QLatin1Char(' '));
    return joined.isEmpty() ? sql.trimmed() : joined;
}

bool QueryHistoryEntry::isMultiline() const {
    return sql.trimmed().contains(QLatin1Char('\n'));
}

HistoryRetention HistoryRetention::clamped(int entries, int days) {
    HistoryRetention r;
    r.maxEntries = std::max(100, entries);
    r.maxDays = std::max(1, days);
    return r;
}

QString HistoryRetention::summary() const {
    return QStringLiteral("keeping the last %1 queries, up to %2 days")
        .arg(QLocale().toString(maxEntries))
        .arg(maxDays);
}

bool HistoryFilter::isEmpty() const {
    return text.trimmed().isEmpty() && connection.isEmpty() &&
           range == HistoryDateRange::All && !outcome.has_value();
}

namespace historyformat {

QString dayTitle(const QDate& date, const QDate& today) {
    if (date == today) {
        return QStringLiteral("Today");
    }
    if (date == today.addDays(-1)) {
        return QStringLiteral("Yesterday");
    }
    const QLocale locale;
    if (date.year() == today.year()) {
        return locale.toString(date, QStringLiteral("dddd d MMMM"));
    }
    return locale.toString(date, QStringLiteral("d MMMM yyyy"));
}

QString time(const QDateTime& dt) {
    return dt.time().toString(QStringLiteral("hh:mm:ss"));
}

QString duration(int ms) {
    if (ms < 1000) {
        return QStringLiteral("%1 ms").arg(ms);
    }
    if (ms < 60000) {
        return QStringLiteral("%1 s").arg(ms / 1000.0, 0, 'f', 2);
    }
    return QStringLiteral("%1 m %2 s")
        .arg(ms / 60000)
        .arg((ms % 60000) / 1000, 2, 10, QLatin1Char('0'));
}

QString rows(const std::optional<int>& n) {
    if (!n.has_value()) {
        return QString();
    }
    return *n == 1 ? QStringLiteral("1 row")
                   : QStringLiteral("%1 rows").arg(QLocale().toString(*n));
}

}  // namespace historyformat
}  // namespace dg

namespace {

// The JSON key names ARE the contract: they match the macOS store's Codable
// output line for line, which itself mirrors the engine's query_history schema.
QJsonObject entryToJson(const dg::QueryHistoryEntry& e) {
    QJsonObject o;
    o.insert(QStringLiteral("id"), e.id);
    o.insert(QStringLiteral("sql"), e.sql);
    o.insert(QStringLiteral("connection"), e.connection);
    o.insert(QStringLiteral("engine"), e.engine);
    o.insert(QStringLiteral("startedAtMs"), e.startedAtMs);
    o.insert(QStringLiteral("durationMs"), e.durationMs);
    if (e.rowCount.has_value()) {
        o.insert(QStringLiteral("rowCount"), *e.rowCount);
    }
    if (e.affectedRows.has_value()) {
        o.insert(QStringLiteral("affectedRows"), *e.affectedRows);
    }
    o.insert(QStringLiteral("outcome"), dg::outcomeKey(e.outcome));
    if (!e.error.isEmpty()) {
        o.insert(QStringLiteral("error"), e.error);
    }
    o.insert(QStringLiteral("runCount"), e.runCount);
    o.insert(QStringLiteral("textHash"), e.textHash);
    return o;
}

// Defensive: a hand-edited or truncated line degrades to "skip this entry",
// never to losing the file.
std::optional<dg::QueryHistoryEntry> entryFromJson(const QJsonObject& o) {
    dg::QueryHistoryEntry e;
    e.sql = o.value(QStringLiteral("sql")).toString();
    if (e.sql.trimmed().isEmpty()) {
        return std::nullopt;
    }
    e.id = o.value(QStringLiteral("id")).toString();
    if (e.id.isEmpty()) {
        e.id = QUuid::createUuid().toString(QUuid::WithoutBraces);
    }
    e.connection = o.value(QStringLiteral("connection")).toString();
    e.engine = o.value(QStringLiteral("engine")).toString();
    e.startedAtMs =
        static_cast<qint64>(o.value(QStringLiteral("startedAtMs")).toDouble(0));
    e.durationMs = o.value(QStringLiteral("durationMs")).toInt(0);
    if (o.contains(QStringLiteral("rowCount"))) {
        e.rowCount = o.value(QStringLiteral("rowCount")).toInt(0);
    }
    if (o.contains(QStringLiteral("affectedRows"))) {
        e.affectedRows = o.value(QStringLiteral("affectedRows")).toInt(0);
    }
    e.outcome = dg::outcomeFromKey(o.value(QStringLiteral("outcome")).toString())
                    .value_or(dg::QueryOutcome::Ok);
    e.error = o.value(QStringLiteral("error")).toString();
    e.runCount = std::max(1, o.value(QStringLiteral("runCount")).toInt(1));
    e.textHash = o.value(QStringLiteral("textHash")).toString();
    if (e.textHash.isEmpty()) {
        e.textHash = QueryHistoryStore::hashText(e.sql);
    }
    return e;
}

QString retentionPath(const QString& dir) {
    return dir + QStringLiteral("/retention.json");
}

dg::HistoryRetention readRetention(const QString& dir) {
    QFile f(retentionPath(dir));
    if (!f.open(QIODevice::ReadOnly)) {
        return dg::HistoryRetention{};
    }
    const QJsonObject o = QJsonDocument::fromJson(f.readAll()).object();
    // Clamped on read too — a retention.json hand-edited to 0 must not be read
    // as "delete everything".
    return dg::HistoryRetention::clamped(
        o.value(QStringLiteral("maxEntries")).toInt(10000),
        o.value(QStringLiteral("maxDays")).toInt(180));
}

void writeRetention(const dg::HistoryRetention& r, const QString& dir) {
    QJsonObject o;
    o.insert(QStringLiteral("maxEntries"), r.maxEntries);
    o.insert(QStringLiteral("maxDays"), r.maxDays);
    QSaveFile f(retentionPath(dir));
    if (!f.open(QIODevice::WriteOnly)) {
        return;
    }
    f.write(QJsonDocument(o).toJson(QJsonDocument::Indented));
    f.commit();
}

void sortNewestFirst(QVector<dg::QueryHistoryEntry>& entries) {
    std::stable_sort(entries.begin(), entries.end(),
                     [](const dg::QueryHistoryEntry& a, const dg::QueryHistoryEntry& b) {
                         return a.startedAtMs > b.startedAtMs;
                     });
}

}  // namespace

QueryHistoryStore::QueryHistoryStore(const QString& directory, QObject* parent)
    : QObject(parent), directory_(directory) {
    QDir().mkpath(directory_);
    retention_ = readRetention(directory_);
    flushTimer_.setSingleShot(true);
    flushTimer_.setInterval(600);
    connect(&flushTimer_, &QTimer::timeout, this, &QueryHistoryStore::flush);
}

QueryHistoryStore::~QueryHistoryStore() {
    // A statement run moments before quit must not be lost to the debounce.
    if (flushTimer_.isActive() || !dirtyDays_.isEmpty()) {
        flush();
    }
}

QString QueryHistoryStore::defaultDirectory() {
    return QStandardPaths::writableLocation(QStandardPaths::AppDataLocation) +
           QStringLiteral("/history");
}

void QueryHistoryStore::load() {
    if (loaded_) {
        return;
    }
    loaded_ = true;
    QDir dir(directory_);
    // The filenames are ISO dates, so a reversed name sort is newest-first —
    // and newest-first means reading can stop as soon as the entry budget is
    // met; older files stay on disk until pruned.
    const QStringList files = dir.entryList({QStringLiteral("*.jsonl")}, QDir::Files,
                                            QDir::Name | QDir::Reversed);
    const QString cutoff = cutoffDayKey(retention_.maxDays);
    QVector<dg::QueryHistoryEntry> loadedEntries;
    for (const QString& name : files) {
        const QString key = name.left(name.size() - 6);  // strip ".jsonl"
        if (key < cutoff) {
            QFile::remove(dir.filePath(name));
            continue;
        }
        QFile f(dir.filePath(name));
        if (!f.open(QIODevice::ReadOnly | QIODevice::Text)) {
            continue;
        }
        while (!f.atEnd()) {
            const QByteArray line = f.readLine().trimmed();
            if (line.isEmpty()) {
                continue;
            }
            const QJsonDocument doc = QJsonDocument::fromJson(line);
            if (!doc.isObject()) {
                continue;
            }
            if (auto e = entryFromJson(doc.object())) {
                loadedEntries.append(*e);
            }
        }
        if (loadedEntries.size() >= retention_.maxEntries) {
            break;
        }
    }
    sortNewestFirst(loadedEntries);
    entries_ = loadedEntries;
    prune();
    emit changed();
}

void QueryHistoryStore::record(dg::QueryHistoryEntry entry) {
    if (entry.sql.trimmed().isEmpty()) {
        return;
    }
    if (entry.id.isEmpty()) {
        entry.id = QUuid::createUuid().toString(QUuid::WithoutBraces);
    }
    if (entry.textHash.isEmpty()) {
        entry.textHash = hashText(entry.sql);
    }

    // Dedupe: the most recent entry with the same statement on the same
    // connection, inside the window, with the same outcome AND the same error.
    int found = -1;
    for (int i = 0; i < entries_.size(); ++i) {
        const dg::QueryHistoryEntry& e = entries_.at(i);
        if (e.textHash == entry.textHash && e.connection == entry.connection &&
            e.outcome == entry.outcome && e.error == entry.error &&
            qAbs(entry.startedAtMs - e.startedAtMs) <= kDedupeWindowMs) {
            found = i;
            break;
        }
    }
    if (found >= 0) {
        dg::QueryHistoryEntry merged = entries_.at(found);
        dirtyDays_.insert(merged.dayKey());  // it may be leaving this day
        merged.startedAtMs = entry.startedAtMs;
        merged.durationMs = entry.durationMs;
        merged.rowCount = entry.rowCount;
        merged.affectedRows = entry.affectedRows;
        merged.runCount += 1;
        entries_.removeAt(found);
        entries_.prepend(merged);
        dirtyDays_.insert(merged.dayKey());
    } else {
        entries_.prepend(entry);
        dirtyDays_.insert(entry.dayKey());
    }

    sortNewestFirst(entries_);
    prune();
    scheduleFlush();
    emit changed();
}

void QueryHistoryStore::remove(const QSet<QString>& ids) {
    if (ids.isEmpty()) {
        return;
    }
    for (const dg::QueryHistoryEntry& e : std::as_const(entries_)) {
        if (ids.contains(e.id)) {
            dirtyDays_.insert(e.dayKey());
        }
    }
    entries_.removeIf(
        [&ids](const dg::QueryHistoryEntry& e) { return ids.contains(e.id); });
    scheduleFlush();
    emit changed();
}

void QueryHistoryStore::clear(const QString& connection) {
    if (connection.isEmpty()) {
        for (const dg::QueryHistoryEntry& e : std::as_const(entries_)) {
            dirtyDays_.insert(e.dayKey());
        }
        entries_.clear();
    } else {
        for (const dg::QueryHistoryEntry& e : std::as_const(entries_)) {
            if (e.connection == connection) {
                dirtyDays_.insert(e.dayKey());
            }
        }
        entries_.removeIf([&connection](const dg::QueryHistoryEntry& e) {
            return e.connection == connection;
        });
    }
    scheduleFlush();
    emit changed();
}

void QueryHistoryStore::setRetention(dg::HistoryRetention r) {
    retention_ = dg::HistoryRetention::clamped(r.maxEntries, r.maxDays);
    writeRetention(retention_, directory_);
    prune();
    scheduleFlush();
    emit changed();
}

void QueryHistoryStore::executionStarted(const QString& sql, const QString& connection,
                                         const QString& engine) {
    if (sql.trimmed().isEmpty()) {
        pending_.reset();
        return;
    }
    pending_ = PendingRun{sql, connection, engine, QDateTime::currentDateTime(), false};
}

void QueryHistoryStore::executionProgressed(const dg::QueryStatus& status) {
    if (!pending_ || pending_->recorded || !dg::isTerminal(status.state)) {
        return;
    }
    pending_->recorded = true;

    dg::QueryOutcome outcome = dg::QueryOutcome::Ok;
    if (status.state == dg::QueryState::Failed) {
        outcome = dg::QueryOutcome::Error;
    } else if (status.state == dg::QueryState::Cancelled) {
        outcome = dg::QueryOutcome::Cancelled;
    }
    commitPending(
        outcome,
        static_cast<int>(qMin<quint64>(status.elapsedMs, INT_MAX)),
        static_cast<int>(qMin<quint64>(status.rowsLoaded, INT_MAX)),
        outcome == dg::QueryOutcome::Error ? status.error : QString());
}

void QueryHistoryStore::executionFailedToStart(const QString& message) {
    if (!pending_ || pending_->recorded) {
        return;
    }
    pending_->recorded = true;
    commitPending(
        dg::QueryOutcome::Error,
        static_cast<int>(pending_->startedAt.msecsTo(QDateTime::currentDateTime())),
        std::nullopt, message);
}

void QueryHistoryStore::commitPending(dg::QueryOutcome outcome, int durationMs,
                                      std::optional<int> rowCount, const QString& error) {
    dg::QueryHistoryEntry e;
    e.sql = pending_->sql;
    e.connection = pending_->connection;
    e.engine = pending_->engine;
    e.startedAtMs = pending_->startedAt.toMSecsSinceEpoch();
    e.durationMs = durationMs;
    e.rowCount = rowCount;
    e.outcome = outcome;
    e.error = error;
    record(std::move(e));
}

QString QueryHistoryStore::normalise(const QString& sql) {
    QString out;
    out.reserve(sql.size());
    bool lastWasSpace = false;
    for (const QChar ch : sql) {
        if (ch.isSpace()) {
            if (!lastWasSpace && !out.isEmpty()) {
                out.append(QLatin1Char(' '));
            }
            lastWasSpace = true;
        } else {
            out.append(ch);
            lastWasSpace = false;
        }
    }
    while (out.endsWith(QLatin1Char(' ')) || out.endsWith(QLatin1Char(';'))) {
        out.chop(1);
    }
    return out;
}

QString QueryHistoryStore::hashText(const QString& sql) {
    const QByteArray bytes = normalise(sql).toUtf8();
    quint64 h = Q_UINT64_C(0xcbf29ce484222325);
    for (const char c : bytes) {
        h ^= static_cast<quint64>(static_cast<unsigned char>(c));
        h *= Q_UINT64_C(0x100000001b3);
    }
    return QString::number(h, 16);
}

QVector<dg::QueryHistoryEntry> QueryHistoryStore::filter(
    const QVector<dg::QueryHistoryEntry>& entries, const dg::HistoryFilter& f,
    const QDateTime& now) {
    std::optional<QDateTime> earliest;
    switch (f.range) {
        case dg::HistoryDateRange::All: break;
        case dg::HistoryDateRange::Day: earliest = now.date().startOfDay(); break;
        case dg::HistoryDateRange::Week: earliest = now.addDays(-7); break;
        case dg::HistoryDateRange::Month: earliest = now.addMonths(-1); break;
    }
    const QStringList terms = f.text.simplified().split(QLatin1Char(' '), Qt::SkipEmptyParts);

    QVector<dg::QueryHistoryEntry> out;
    for (const dg::QueryHistoryEntry& e : entries) {
        if (!f.connection.isEmpty() && e.connection != f.connection) {
            continue;
        }
        if (f.outcome.has_value() && e.outcome != *f.outcome) {
            continue;
        }
        if (earliest.has_value() && e.startedAt() < *earliest) {
            continue;
        }
        // AND across terms: typing more words narrows. The error text is
        // searched too — "deadlock" should find the query that hit one.
        bool matches = true;
        for (const QString& t : terms) {
            if (!e.sql.contains(t, Qt::CaseInsensitive) &&
                !e.error.contains(t, Qt::CaseInsensitive)) {
                matches = false;
                break;
            }
        }
        if (matches) {
            out.append(e);
        }
    }
    return out;
}

QVector<dg::HistoryDay> QueryHistoryStore::group(
    const QVector<dg::QueryHistoryEntry>& entries, const QDate& today) {
    QVector<dg::HistoryDay> days;
    for (const dg::QueryHistoryEntry& e : entries) {
        const QString key = e.dayKey();
        if (days.isEmpty() || days.last().key != key) {
            dg::HistoryDay day;
            day.key = key;
            day.title = dg::historyformat::dayTitle(
                QDate::fromString(key, Qt::ISODate), today);
            days.append(day);
        }
        days.last().entries.append(e);
    }
    return days;
}

QStringList QueryHistoryStore::connections(const QVector<dg::QueryHistoryEntry>& entries) {
    QSet<QString> names;
    for (const dg::QueryHistoryEntry& e : entries) {
        if (!e.connection.isEmpty()) {
            names.insert(e.connection);
        }
    }
    QStringList out(names.cbegin(), names.cend());
    out.sort();
    return out;
}

void QueryHistoryStore::prune() {
    if (entries_.size() > retention_.maxEntries) {
        for (int i = retention_.maxEntries; i < entries_.size(); ++i) {
            dirtyDays_.insert(entries_.at(i).dayKey());
        }
        entries_.resize(retention_.maxEntries);
    }
    const QString cutoff = cutoffDayKey(retention_.maxDays);
    int firstStale = -1;
    for (int i = 0; i < entries_.size(); ++i) {
        if (entries_.at(i).dayKey() < cutoff) {
            firstStale = i;
            break;
        }
    }
    if (firstStale >= 0) {
        for (int i = firstStale; i < entries_.size(); ++i) {
            dirtyDays_.insert(entries_.at(i).dayKey());
        }
        entries_.resize(firstStale);
    }
}

void QueryHistoryStore::scheduleFlush() {
    if (!flushTimer_.isActive()) {
        flushTimer_.start();
    }
}

void QueryHistoryStore::flush() {
    flushTimer_.stop();
    QHash<QString, QVector<const dg::QueryHistoryEntry*>> byDay;
    for (const dg::QueryHistoryEntry& e : std::as_const(entries_)) {
        byDay[e.dayKey()].append(&e);
    }

    // Rewrite only the days that changed.
    for (const QString& day : std::as_const(dirtyDays_)) {
        const QString path = directory_ + QStringLiteral("/") + day +
                             QStringLiteral(".jsonl");
        const auto it = byDay.constFind(day);
        if (it == byDay.constEnd() || it->isEmpty()) {
            QFile::remove(path);
            continue;
        }
        QSaveFile f(path);
        if (!f.open(QIODevice::WriteOnly | QIODevice::Text)) {
            continue;
        }
        // Oldest first inside a file, so it reads naturally with `tail`.
        for (auto rit = it->crbegin(); rit != it->crend(); ++rit) {
            f.write(QJsonDocument(entryToJson(**rit)).toJson(QJsonDocument::Compact));
            f.write("\n");
        }
        f.commit();
    }
    dirtyDays_.clear();

    // Drop any day file retention has outlived. Derived from what is actually
    // in memory, so a stale or hand-edited directory heals itself.
    const QString cutoff = cutoffDayKey(retention_.maxDays);
    QDir dir(directory_);
    const QStringList files = dir.entryList({QStringLiteral("*.jsonl")}, QDir::Files);
    for (const QString& name : files) {
        const QString key = name.left(name.size() - 6);
        if (key < cutoff && !byDay.contains(key)) {
            QFile::remove(dir.filePath(name));
        }
    }
}

// `days` counts inclusive of today: a retention of 1 keeps today and nothing
// else — "keep 1 day" that quietly kept two would be an undocumented cap.
QString QueryHistoryStore::cutoffDayKey(int days, const QDate& today) {
    return today.addDays(-(std::max(1, days) - 1)).toString(Qt::ISODate);
}
