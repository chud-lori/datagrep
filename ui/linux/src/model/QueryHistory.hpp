// QueryHistory.hpp — the automatic log of every statement datagrep actually ran.
//
// Linux counterpart of DatagrepKit.QueryHistory. The engine's `query_history`
// table (crates/datagrep-profiles, FTS5) is still unreachable over the C ABI —
// there is no datagrep_history_* entry point — so this store mirrors that schema
// field-for-field and persists it the way the macOS store does: one JSON-lines
// file per day, same key names, plus `retention.json`. When the ABI grows a
// history surface the move is a copy, not a redesign.
//
// The design commitments shared with macOS, kept deliberately:
//  * history is never scoped to the current connection — connection is a filter
//    the user may apply, not one applied for them;
//  * retention is stated and editable, never a silent cap;
//  * failures are kept, with their error;
//  * the engine (driver id) is stored on the entry, so a deleted connection
//    still has a readable past.

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

// Same three cases, same stored spellings, as the engine's HistoryStatus CHECK
// constraint and the macOS QueryOutcome.
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

    // 0 or negative would silently mean "keep nothing" — clamp instead of
    // quietly deleting history because a field was left empty.
    static HistoryRetention clamped(int entries, int days);
    // The sentence the panel shows. Retention the user cannot read is the same
    // as retention the user cannot set.
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
// GUI-thread only; writes are debounced through one single-shot timer so the
// run path never waits on the disk.
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

    // Loads everything inside retention, newest first. Idempotent: a second
    // load would re-read the last *flushed* state and silently drop anything
    // recorded inside the debounce window.
    void load();

    void record(dg::QueryHistoryEntry entry);
    void remove(const QSet<QString>& ids);
    // Clears everything, or everything for one connection. Destructive and
    // explicit — nothing here ever clears history as a side effect.
    void clear(const QString& connection = QString());
    // Prunes immediately — a setting that only takes effect "eventually" is a
    // setting the user cannot verify.
    void setRetention(dg::HistoryRetention r);

    // Run-path hooks. executionStarted costs four string copies; the entry is
    // committed exactly once, when the query reaches a terminal state, so a
    // minute of streaming makes one entry, not one per progress tick.
    void executionStarted(const QString& sql, const QString& connection,
                          const QString& engine);
    void executionProgressed(const dg::QueryStatus& status);
    // The run never got a query handle (connect failure, rejected statement).
    // Recorded like any other: these are the entries people most want back.
    void executionFailedToStart(const QString& message);

    // Whitespace collapsed, trailing semicolons dropped, case KEPT (identifiers
    // are case-sensitive on several engines).
    static QString normalise(const QString& sql);
    // FNV-1a, not qHash: qHash is seeded per process, and dedupe must survive
    // restarts. Same function, same hex form, as the macOS store.
    static QString hashText(const QString& sql);

    static QVector<dg::QueryHistoryEntry> filter(
        const QVector<dg::QueryHistoryEntry>& entries, const dg::HistoryFilter& f,
        const QDateTime& now = QDateTime::currentDateTime());
    static QVector<dg::HistoryDay> group(const QVector<dg::QueryHistoryEntry>& entries,
                                         const QDate& today = QDate::currentDate());
    // Connection names that appear in history — from the entries, not the live
    // profile list: a deleted connection still has a past worth filtering to.
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

    // Re-running the same statement on the same connection inside this window
    // updates the entry in place. A different outcome always makes a new entry:
    // the run that worked and the run that failed are two events.
    static constexpr qint64 kDedupeWindowMs = 120 * 1000;
};

#endif  // DATAGREP_QUERY_HISTORY_HPP
