// QueryHistory.hpp — the automatic log of every statement datagrep actually ran.

#ifndef DATAGREP_QUERY_HISTORY_HPP
#define DATAGREP_QUERY_HISTORY_HPP

#include "QueryStatus.hpp"

#include <QDate>
#include <QDateTime>
#include <QObject>
#include <QSet>
#include <QString>
#include <QStringList>
#include <QTimer>
#include <QVector>

#include <optional>

namespace dg {

enum class QueryOutcome { Ok, Error, Cancelled };

QString outcomeKey(QueryOutcome o);    // "ok" | "error" | "cancelled" (stored)
QString outcomeLabel(QueryOutcome o);  // "ok" | "failed" | "cancelled" (shown)
std::optional<QueryOutcome> outcomeFromKey(const QString& key);

struct QueryHistoryEntry {
    QString id;
    QString sql;         // verbatim as the user ran it — never reformatted
    QString connection;  // profile name; empty only if none was selected
    QString engine;      // driver id, kept so a deleted connection still reads
    qint64 startedAtMs = 0;
    int durationMs = 0;
    std::optional<int> rowCount;      // nullopt when no result set
    std::optional<int> affectedRows;  // always nullopt: no Shape::Ack in this ABI yet
    QueryOutcome outcome = QueryOutcome::Ok;
    QString error;    // empty = none; a failed query is worthless without it
    int runCount = 1;  // dedupe-window collapses land here
    QString textHash;  // FNV-1a over the normalised SQL; stable across launches

    QDateTime startedAt() const { return QDateTime::fromMSecsSinceEpoch(startedAtMs); }
    // Day bucket in the user's own time zone — "Today" means the day they had.
    QString dayKey() const { return startedAt().date().toString(Qt::ISODate); }
    // Whitespace collapsed for the list row; the full text stays in `sql`.
    QString oneLine() const;
    bool isMultiline() const;
};

struct HistoryRetention {
    int maxEntries = 10000;
    int maxDays = 180;

    static HistoryRetention clamped(int entries, int days);
    QString summary() const;
};

enum class HistoryDateRange { Day, Week, Month, All };

struct HistoryFilter {
    QString text;
    QString connection;  // empty = every connection
    HistoryDateRange range = HistoryDateRange::All;
    std::optional<QueryOutcome> outcome;

    bool isEmpty() const;
};

struct HistoryDay {
    QString key;    // yyyy-MM-dd
    QString title;  // "Today" / "Yesterday" / a readable date
    QVector<QueryHistoryEntry> entries;
};

namespace historyformat {
QString dayTitle(const QDate& date, const QDate& today = QDate::currentDate());
QString time(const QDateTime& dt);
QString duration(int ms);
QString rows(const std::optional<int>& n);  // empty when no result set
}  // namespace historyformat

}  // namespace dg

// Reads and writes the history directory. Pure file I/O — no engine, no ABI.
class QueryHistoryStore : public QObject {
    Q_OBJECT

public:
    explicit QueryHistoryStore(const QString& directory = defaultDirectory(),
                               QObject* parent = nullptr);
    ~QueryHistoryStore() override;

    // <app data>/history/, beside the engine's profiles store.
    static QString defaultDirectory();

    const QString& directory() const { return directory_; }
    const QVector<dg::QueryHistoryEntry>& entries() const { return entries_; }
    dg::HistoryRetention retention() const { return retention_; }

    void load();

    void record(dg::QueryHistoryEntry entry);
    void remove(const QSet<QString>& ids);
    void clear(const QString& connection = QString());
    void setRetention(dg::HistoryRetention r);

    void executionStarted(const QString& sql, const QString& connection,
                          const QString& engine);
    void executionProgressed(const dg::QueryStatus& status);
    // The run never got a query handle (connect failure, rejected statement).
    void executionFailedToStart(const QString& message);

    static QString normalise(const QString& sql);
    static QString hashText(const QString& sql);

    static QVector<dg::QueryHistoryEntry> filter(
        const QVector<dg::QueryHistoryEntry>& entries, const dg::HistoryFilter& f,
        const QDateTime& now = QDateTime::currentDateTime());
    static QVector<dg::HistoryDay> group(const QVector<dg::QueryHistoryEntry>& entries,
                                         const QDate& today = QDate::currentDate());
    static QStringList connections(const QVector<dg::QueryHistoryEntry>& entries);

signals:
    void changed();

private:
    struct PendingRun {
        QString sql;
        QString connection;
        QString engine;
        QDateTime startedAt;
        bool recorded = false;
    };

    void commitPending(dg::QueryOutcome outcome, int durationMs,
                       std::optional<int> rowCount, const QString& error);
    void prune();  // entry count first, then age; marks the days it touches dirty
    void scheduleFlush();
    void flush();
    static QString cutoffDayKey(int days, const QDate& today = QDate::currentDate());

    QString directory_;
    QVector<dg::QueryHistoryEntry> entries_;  // newest first
    QSet<QString> dirtyDays_;
    dg::HistoryRetention retention_;
    QTimer flushTimer_;
    std::optional<PendingRun> pending_;
    bool loaded_ = false;

    static constexpr qint64 kDedupeWindowMs = 120 * 1000;
};

#endif  // DATAGREP_QUERY_HISTORY_HPP
