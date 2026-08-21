// QueryStatus.hpp — the decoded form of datagrep_query_status_json().

#ifndef DATAGREP_QUERY_STATUS_HPP
#define DATAGREP_QUERY_STATUS_HPP

#include "Mutation.hpp"

#include <QString>
#include <QStringList>
#include <QVector>

#include <cstdint>
#include <optional>

namespace dg {

enum class QueryState {
    Streaming,
    Parked,
    Capped,
    Done,
    Cancelled,
    Failed,
};

inline QueryState queryStateFromString(const QString& s) {
    if (s == QStringLiteral("streaming")) return QueryState::Streaming;
    if (s == QStringLiteral("parked")) return QueryState::Parked;
    if (s == QStringLiteral("capped")) return QueryState::Capped;
    if (s == QStringLiteral("done")) return QueryState::Done;
    if (s == QStringLiteral("cancelled")) return QueryState::Cancelled;
    return QueryState::Failed;
}

inline bool isTerminal(QueryState s) {
    switch (s) {
        case QueryState::Done:
        case QueryState::Cancelled:
        case QueryState::Failed:
        case QueryState::Capped:
            return true;
        case QueryState::Streaming:
        case QueryState::Parked:
            return false;
    }
    return true;
}

struct ColumnSpec {
    QString name;
    QString type;
};

struct QueryStatus {
    QueryState state = QueryState::Done;
    std::uint64_t rowsLoaded = 0;
    std::optional<std::uint64_t> affectedRows;
    std::uint64_t elapsedMs = 0;
    QString error;  // empty == no error
    QVector<ColumnSpec> columns;
    bool totalKnown = true;

    QString readOnlyEnforcement;  // "server" | "client" | "none" | ""
    bool readOnlyServerConfirmed = false;

    // The "editable" block, when the engine says this result may be edited.
    std::optional<EditableResult> editable;

    bool capped() const { return state == QueryState::Capped; }
    bool streaming() const { return !isTerminal(state); }

    static QueryStatus parse(const QString& json);
};

}  // namespace dg

#endif  // DATAGREP_QUERY_STATUS_HPP
