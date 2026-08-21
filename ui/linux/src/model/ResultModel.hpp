// ResultModel.hpp — a QAbstractTableModel over the datagrep windowed row API.
//
// THIS IS THE CORE OF THE UI. It is virtual by construction, exactly like the
// macOS grid (ResultsViewController + RowPager):
//
//   * A result may be millions of rows. The model NEVER materialises the whole
//     result. data() pulls exactly one cell out of a 512-row page window, and
//     the RowPager keeps at most 4 pages (2,048 rows) resident, freeing evicted
//     windows immediately.
//
//   * QTableView only ever calls data() for the handful of indices it is about
//     to paint, so cost is O(viewport), never O(rows).
//
// Mapping to the C ABI (crates/datagrep-ffi/include/datagrep.h):
//
//   rowCount()      -> the rows currently EXPOSED to the view. Grows toward
//                      status.rows_loaded (from datagrep_query_status_json) as
//                      the background feeder streams; capped by the row-window
//                      reveal mechanism below.
//   columnCount()   -> status "columns" length (datagrep_query_status_json).
//   data()          -> datagrep_query_rows (via RowPager) + datagrep_rows_cell /
//                      datagrep_rows_cell_kind for the addressed cell only.
//   headerData(H)   -> column name/type from status "columns".
//   headerData(V)   -> the 1-based row number (section+1). This is the copy-safe
//                      row-number gutter — see RowNumberHeader.hpp for WHY a row
//                      number returned as vertical headerData can never reach the
//                      pasteboard.
//   canFetchMore()  -> more rows are loaded than exposed, OR the query is still
//                      streaming (so the view keeps polling).
//   fetchMore()     -> re-read datagrep_query_status_json, then reveal the newly
//                      loaded rows with beginInsertRows/endInsertRows.
//
// The progress callback (datagrep_query_on_progress) fires on a BACKGROUND
// thread; onProgress() below marshals it onto the GUI thread with a queued
// signal before touching any model state, then reveals rows the same way
// fetchMore does. fetchMore and the progress tick are the SAME idempotent
// operation (revealLoadedRows), so running both is safe: whichever runs second
// finds nothing left to reveal.

#ifndef DATAGREP_RESULT_MODEL_HPP
#define DATAGREP_RESULT_MODEL_HPP

#include "GridEditing.hpp"
#include "QueryStatus.hpp"
#include "ffi/DatagrepFfi.hpp"
#include "ffi/RowPager.hpp"

#include <QAbstractTableModel>

#include <atomic>
#include <cstdint>
#include <memory>
#include <optional>

class ResultModel : public QAbstractTableModel {
    Q_OBJECT

public:
    // Custom roles the grid delegate / view read alongside the standard ones.
    enum Roles {
        // dg::CellKind as int (0 value, 1 null, 2 absent, 3 nested). Lets a
        // delegate render NULL vs ABSENT vs nested distinctly, the way the macOS
        // grid does — these are different facts, not the same blank cell.
        CellKindRole = Qt::UserRole + 1,
        // true when the addressed row's page has not been fetched yet: draw a
        // skeleton rather than an empty value. (datagrep_rows_pending)
        CellPendingRole,
    };

    explicit ResultModel(QObject* parent = nullptr);
    ~ResultModel() override;

    // Installs a fresh query result. Takes ownership of the Query. Drops the
    // previous result entirely (its pager frees every resident DatagrepRows).
    // Wires the progress callback. Immediately reads the first status snapshot.
    void setQuery(std::unique_ptr<dg::Query> query);

    // Clears everything back to an empty grid (no query).
    void reset();

    // --- QAbstractItemModel ------------------------------------------------
    int rowCount(const QModelIndex& parent = QModelIndex()) const override;
    int columnCount(const QModelIndex& parent = QModelIndex()) const override;
    QVariant data(const QModelIndex& index, int role = Qt::DisplayRole) const override;
    QVariant headerData(int section, Qt::Orientation orientation,
                        int role = Qt::DisplayRole) const override;
    Qt::ItemFlags flags(const QModelIndex& index) const override;
    // The staging path: Qt's edit commit lands here. Stages against the
    // document (never writes); typing the loaded value back un-stages.
    bool setData(const QModelIndex& index, const QVariant& value,
                 int role = Qt::EditRole) override;

    bool canFetchMore(const QModelIndex& parent) const override;
    void fetchMore(const QModelIndex& parent) override;

    // The latest decoded status (for the status bar). Valid after setQuery/any
    // reveal. Also emitted via statusChanged().
    const dg::QueryStatus& status() const { return status_; }

    // Cancel the running query, returning the server's verbatim outcome message
    // (or an empty string if the ABI reported none). Shown to the user as-is.
    QString cancel();

    // Raw JSON detail of one cell, for a detail pane. On-demand, one cell.
    QString cellDetailJson(int row, int column) const;

    // The kind of one loaded cell (NULL / ABSENT / nested / value), or nullopt
    // when the cell's window is not resident. Same refuse-don't-guess rule as
    // cellDetailJson: never triggers a fetch, so the inspector cannot churn the
    // pager by merely being open.
    std::optional<dg::CellKind> cellKind(int row, int column) const;

    // The row's envelope (fields outside the projected root) as JSON, or empty
    // when the driver declared no root or the window is not resident.
    QString envelopeJson(int row) const;

    // --- editing -----------------------------------------------------------
    // The window's veto, set by MainWindow per statement BEFORE the result
    // exists: a read-only connection offers no edit at all, however editable
    // the result itself is — so no window of rows is ever editable for an
    // instant.
    void setAllowsEditing(bool allows) { allowsEditing_ = allows; }
    // The staging store, owned by MainWindow. Held by pointer so the grid can
    // draw staged values without carrying its own copy of them.
    void setPendingEdits(dg::PendingEdits* edits) { edits_ = edits; }
    // What the engine says this result may be edited into, after the veto.
    const std::optional<dg::EditableResult>& editable() const { return editable_; }

    // Stage the document under `row` for deletion / drop its staged edits.
    void stageDeleteRow(int row);
    void discardStagedRow(int row);
    // Repaint rows whose staging changed (after a commit report / a rebase).
    void refreshStagedRows(const QVector<int>& rows);

    // Whether one cell may begin an edit (kind is not nested, window resident).
    bool cellEditable(int row, int column) const;
    bool rowIsStaged(int row) const;
    bool rowIsDeleted(int row) const;

signals:
    // Fired after every status refresh so the status bar can update
    // rows/elapsed/capped/error without reaching into the model.
    void statusChanged(const dg::QueryStatus& status);
    // An edit that could not be staged, in words that name the field and the
    // reason. Never silent: a cell that quietly refuses to keep what was typed
    // is indistinguishable from one that kept it and lost it.
    void editRefused(const QString& why);

private slots:
    // GUI-thread entry point for the background progress callback.
    void onProgressTick();

private:
    // Re-reads status from the ABI and updates cached columns/state/counts.
    // Emits statusChanged. Does NOT itself insert rows.
    void refreshStatus();

    // Reveals more loaded rows to the view. While the feeder is still streaming
    // this exposes at most kFetchBatch rows per call (the QTableView keeps
    // calling fetchMore while there is viewport room). Once the query is terminal
    // it announces ALL remaining rows in one go, so the scrollbar reflects the
    // true total even if the user never scrolled. Idempotent; GUI thread only.
    void revealMore();

    // Grows the exposed row count to `target` via beginInsertRows/endInsertRows.
    void revealTo(std::uint64_t target);

    bool numericType(const QString& type) const;

    // The field name column `col` was read under, from the window ITSELF — not
    // the header: the status reports what the first chunk revealed, and on a
    // heterogeneous result writing by the header would write a field the user
    // never touched. Cached per window; cleared on every status refresh.
    QString fieldName(const dg::RowWindow* window, int col) const;
    // One cell's loaded value, when it is a value an edit can carry (nullopt
    // for nested and absent cells).
    std::optional<dg::MutationValue> loadedValue(const dg::RowWindow* window,
                                                 int row, int col) const;
    // The row's address, or false after emitting editRefused with the reason.
    bool addressRow(int row, const dg::RowWindow* window,
                    dg::EditableResult::Address* out);

    // Rows revealed per fetchMore() call while streaming. Large enough that
    // scrolling never starves, small enough that a mid-stream reveal is cheap.
    static constexpr std::uint64_t kFetchBatch = 4096;

    std::unique_ptr<dg::Query> query_;
    // pager_ borrows *query_; it is reset() before query_ is, so it never
    // outlives it. mutable: data() is const but fetching a window mutates the LRU.
    mutable std::unique_ptr<dg::RowPager> pager_;

    dg::QueryStatus status_;
    std::uint64_t exposedRows_ = 0;  // rows currently visible to the view
    std::uint64_t loadedRows_ = 0;   // status.rows_loaded from the last refresh
    int columnCount_ = 0;
    QStringList columnNames_;
    QStringList columnTypes_;
    QVector<bool> columnRightAligned_;

    bool allowsEditing_ = true;
    std::optional<dg::EditableResult> editable_;
    dg::PendingEdits* edits_ = nullptr;
    // One-entry cache for the current window's projected field names: paints
    // walk cells window by window, and re-parsing the JSON per cell would make
    // every repaint O(columns²). Cleared whenever windows may be replaced.
    mutable const dg::RowWindow* namesWindow_ = nullptr;
    mutable QStringList namesCache_;

    // Coalesces chatty background progress callbacks into one queued GUI-thread
    // tick, mirroring the macOS ProgressBox: while a tick is already queued,
    // further callbacks are dropped so the feeder cannot flood the event loop.
    std::atomic<bool> tickQueued_{false};
};

#endif  // DATAGREP_RESULT_MODEL_HPP
