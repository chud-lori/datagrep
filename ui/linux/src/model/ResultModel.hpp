// ResultModel.hpp — a QAbstractTableModel over the datagrep windowed row API.

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
        CellKindRole = Qt::UserRole + 1,
        // true when the row's page has not been fetched yet: draw a skeleton.
        CellPendingRole,
    };

    explicit ResultModel(QObject* parent = nullptr);
    ~ResultModel() override;

    // Takes ownership, drops the previous result, wires the progress callback.
    void setQuery(std::unique_ptr<dg::Query> query);

    void reset();

    // --- QAbstractItemModel ------------------------------------------------
    int rowCount(const QModelIndex& parent = QModelIndex()) const override;
    int columnCount(const QModelIndex& parent = QModelIndex()) const override;
    QVariant data(const QModelIndex& index, int role = Qt::DisplayRole) const override;
    QVariant headerData(int section, Qt::Orientation orientation,
                        int role = Qt::DisplayRole) const override;
    Qt::ItemFlags flags(const QModelIndex& index) const override;
    bool setData(const QModelIndex& index, const QVariant& value,
                 int role = Qt::EditRole) override;

    bool canFetchMore(const QModelIndex& parent) const override;
    void fetchMore(const QModelIndex& parent) override;

    // Latest decoded status; also emitted via statusChanged().
    const dg::QueryStatus& status() const { return status_; }

    // Cancel the running query, returning the server's verbatim outcome message.
    QString cancel();

    // Raw JSON detail of one cell, for the detail pane.
    QString cellDetailJson(int row, int column) const;

    // Kind of one loaded cell, or nullopt when its window is not resident.
    std::optional<dg::CellKind> cellKind(int row, int column) const;

    // The row's envelope (fields outside the projected root) as JSON, or empty.
    QString envelopeJson(int row) const;

    void setAllowsEditing(bool allows) { allowsEditing_ = allows; }
    // Staging store, owned by MainWindow.
    void setPendingEdits(dg::PendingEdits* edits) { edits_ = edits; }
    // What the engine says this result may be edited into, after the veto.
    const std::optional<dg::EditableResult>& editable() const { return editable_; }

    void stageDeleteRow(int row);
    void discardStagedRow(int row);
    // Repaint rows whose staging changed (after a commit report / a rebase).
    void refreshStagedRows(const QVector<int>& rows);

    bool cellEditable(int row, int column) const;
    bool rowIsStaged(int row) const;
    bool rowIsDeleted(int row) const;

signals:
    void statusChanged(const dg::QueryStatus& status);
    void editRefused(const QString& why);

private slots:
    void onProgressTick();

private:
    // Re-reads status from the ABI and emits statusChanged. Does NOT insert rows.
    void refreshStatus();

    void revealMore();

    void revealTo(std::uint64_t target);

    bool numericType(const QString& type) const;

    // From the window itself, never the status header — heterogeneous results differ.
    QString fieldName(const dg::RowWindow* window, int col) const;
    // One cell's loaded value (nullopt for nested and absent cells).
    std::optional<dg::MutationValue> loadedValue(const dg::RowWindow* window,
                                                 int row, int col) const;
    // The row's address, or false after emitting editRefused with the reason.
    bool addressRow(int row, const dg::RowWindow* window,
                    dg::EditableResult::Address* out);

    // Rows revealed per fetchMore() call while streaming.
    static constexpr std::uint64_t kFetchBatch = 4096;

    std::unique_ptr<dg::Query> query_;
    // Borrows *query_ and is reset() first; mutable because fetching updates the LRU.
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
    mutable const dg::RowWindow* namesWindow_ = nullptr;
    mutable QStringList namesCache_;

    // Drops progress callbacks while a tick is queued so the feeder cannot flood the GUI loop.
    std::atomic<bool> tickQueued_{false};
};

#endif  // DATAGREP_RESULT_MODEL_HPP
