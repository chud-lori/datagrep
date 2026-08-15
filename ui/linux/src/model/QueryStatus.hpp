// QueryStatus.hpp — the decoded form of datagrep_query_status_json().
//
// The ABI returns status as JSON (contract quoted below); this struct is what the
// model and status bar consume. Mirrors DatagrepKit.QueryStatus. Parsing lives
// here (Qt's QJson) rather than in the dependency-free FFI layer.
//
// ABI contract (crates/datagrep-ffi/include/datagrep.h):
//   {"state":"streaming"|"parked"|"capped"|"done"|"cancelled"|"failed",
//    "rows_loaded":u64,"affected_rows":u64|null,"elapsed_ms":u64,
//    "error":string|null,
//    "read_only": null | {"enforcement":"server"|"client"|"none",
//                         "server_confirmed":bool},
//    "columns":[{"name":..,"type":..}],"total_known":bool}

#ifndef DATAGREP_QUERY_STATUS_HPP
#define DATAGREP_QUERY_STATUS_HPP

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

// Terminal states: the feeder has stopped. `capped` is terminal too — the server
// hit a cap and there are honestly no more rows to wait for.
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

    // read_only enforcement, if the profile is guarded. Kept as the honest
    // "which protection is in force" string the ABI reports; empty when the
    // profile is writeable.
    QString readOnlyEnforcement;  // "server" | "client" | "none" | ""
    bool readOnlyServerConfirmed = false;

    bool capped() const { return state == QueryState::Capped; }
    bool streaming() const { return !isTerminal(state); }

    // Parses one status JSON payload. Defensive: missing keys degrade rather than
    // throw, because the payload grows new keys over time.
    static QueryStatus parse(const QString& json);
};

}  // namespace dg

#endif  // DATAGREP_QUERY_STATUS_HPP
