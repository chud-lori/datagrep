#include "ResultModel.hpp"

#include <QBrush>
#include <QColor>
#include <QMetaObject>
#include <QPalette>

#include <algorithm>
#include <limits>

ResultModel::ResultModel(QObject* parent) : QAbstractTableModel(parent) {}

ResultModel::~ResultModel() {
    // Order matters: drop the pager (frees resident DatagrepRows) before the
    // Query it borrows. query_'s destructor then joins the feeder.
    pager_.reset();
    query_.reset();
}

void ResultModel::setQuery(std::unique_ptr<dg::Query> query) {
    beginResetModel();
    // Tear down the old result first: pager before query.
    pager_.reset();
    query_ = std::move(query);
    exposedRows_ = 0;
    loadedRows_ = 0;
    columnCount_ = 0;
    columnNames_.clear();
    columnTypes_.clear();
    columnRightAligned_.clear();
    status_ = dg::QueryStatus{};
    if (query_) {
        pager_ = std::make_unique<dg::RowPager>(*query_);
        // Marshal the background progress callback onto the GUI thread. The ABI
        // fires the callback from a feeder thread; a queued invocation of
        // onProgressTick() is our equivalent of the macOS DispatchQueue.main hop.
        // Capturing `this` is safe: the Query (and thus the callback) is destroyed
        // in this object's destructor, before `this` becomes invalid.
        query_->onProgress([this]() {
            // Runs on a foreign tokio thread — do the ABSOLUTE MINIMUM here and
            // never touch model state. Coalesce, then hop to the GUI thread.
            if (tickQueued_.exchange(true)) {
                return;  // a tick is already queued; drop this one
            }
            QMetaObject::invokeMethod(this, "onProgressTick", Qt::QueuedConnection);
        });
    }
    endResetModel();

    // First snapshot: columns may already be known and some rows already loaded.
    if (query_) {
        refreshStatus();
        revealMore();
    }
}

void ResultModel::reset() {
    beginResetModel();
    pager_.reset();
    query_.reset();
    exposedRows_ = 0;
    loadedRows_ = 0;
    columnCount_ = 0;
    columnNames_.clear();
    columnTypes_.clear();
    columnRightAligned_.clear();
    status_ = dg::QueryStatus{};
    endResetModel();
    emit statusChanged(status_);
}

int ResultModel::rowCount(const QModelIndex& parent) const {
    if (parent.isValid()) {
        return 0;  // a table model has no child rows
    }
    return static_cast<int>(exposedRows_);
}

int ResultModel::columnCount(const QModelIndex& parent) const {
    if (parent.isValid()) {
        return 0;
    }
    return columnCount_;
}

QVariant ResultModel::data(const QModelIndex& index, int role) const {
    if (!index.isValid() || !pager_) {
        return QVariant();
    }
    const int row = index.row();
    const int col = index.column();
    if (row < 0 || static_cast<std::uint64_t>(row) >= exposedRows_ || col < 0 ||
        col >= columnCount_) {
        return QVariant();
    }

    const auto absRow = static_cast<std::uint64_t>(row);
    const auto absCol = static_cast<std::uint32_t>(col);

    // A window fetch can throw (ABI error mid-stream). Never let it escape into
    // Qt's paint loop; degrade to a blank/skeleton cell.
    const dg::RowWindow* window = nullptr;
    try {
        window = pager_->window(absRow);
    } catch (const dg::Error&) {
        window = nullptr;
    }

    // A window narrower than the announced schema (fetched before a schema
    // delta appended columns) must never be asked for a column it does not
    // have: the ABI indexes row*cols+col into a flat cell array, so an
    // out-of-range column returns ANOTHER ROW'S cell, not an error. Render as
    // pending instead — refreshStatus() already dropped such windows, and the
    // re-fetch comes back full-width.
    if (window != nullptr && absCol >= window->columns()) {
        window = nullptr;
    }

    if (window == nullptr) {
        // Page not available yet: draw a skeleton for the pending case.
        switch (role) {
            case CellPendingRole:
                return true;
            case Qt::DisplayRole:
                return QString();
            default:
                return QVariant();
        }
    }

    const dg::CellKind kind = window->kind(absRow, absCol);

    switch (role) {
        case Qt::DisplayRole: {
            switch (kind) {
                case dg::CellKind::Null:
                    return QStringLiteral("NULL");
                case dg::CellKind::Absent:
                    // Field genuinely not present in this document — render blank,
                    // distinct from NULL. The delegate can style it via CellKindRole.
                    return QString();
                case dg::CellKind::Value:
                case dg::CellKind::Nested:
                    // Nested cells already come back as a summary ("{3 fields}").
                    return QString::fromStdString(window->cellText(absRow, absCol));
            }
            return QString();
        }
        case CellKindRole:
            return static_cast<int>(kind);
        case CellPendingRole:
            return window->pending();
        case Qt::TextAlignmentRole:
            if (col < columnRightAligned_.size() && columnRightAligned_[col]) {
                return static_cast<int>(Qt::AlignRight | Qt::AlignVCenter);
            }
            return static_cast<int>(Qt::AlignLeft | Qt::AlignVCenter);
        case Qt::ForegroundRole:
            // NULL / ABSENT / nested read as chrome, not data — muted like the
            // macOS grid's placeholder colour.
            if (kind == dg::CellKind::Null || kind == dg::CellKind::Absent ||
                kind == dg::CellKind::Nested) {
                return QBrush(QColor(0x88, 0x88, 0x88));
            }
            return QVariant();
        case Qt::ToolTipRole:
            // On-demand raw JSON for a single cell — never bulk.
            if (kind == dg::CellKind::Nested || kind == dg::CellKind::Value) {
                if (auto detail = window->cellDetailJson(absRow, absCol)) {
                    return QString::fromStdString(*detail);
                }
            }
            return QVariant();
        default:
            return QVariant();
    }
}

QVariant ResultModel::headerData(int section, Qt::Orientation orientation,
                                 int role) const {
    if (orientation == Qt::Horizontal) {
        if (section < 0 || section >= columnCount_) {
            return QVariant();
        }
        switch (role) {
            case Qt::DisplayRole:
                return columnNames_.value(section);
            case Qt::ToolTipRole:
                return columnTypes_.value(section);
            case Qt::TextAlignmentRole:
                return static_cast<int>(Qt::AlignLeft | Qt::AlignVCenter);
            default:
                return QVariant();
        }
    }

    // Vertical header == the row-number gutter. Returning the 1-based row number
    // here is the WHOLE copy-safety story: a vertical-header value is not a
    // QModelIndex, is never part of selectedIndexes(), and our copy path only
    // ever serialises selectedIndexes(). The number is therefore STRUCTURALLY
    // incapable of appearing in copied output. See RowNumberHeader.hpp.
    if (orientation == Qt::Vertical) {
        if (role == Qt::DisplayRole) {
            if (section < 0 || static_cast<std::uint64_t>(section) >= exposedRows_) {
                return QVariant();
            }
            // 1-based, ungrouped ("4821", not "4,821"), matching the macOS gutter.
            return QString::number(static_cast<qulonglong>(section) + 1);
        }
        if (role == Qt::TextAlignmentRole) {
            return static_cast<int>(Qt::AlignRight | Qt::AlignVCenter);
        }
    }
    return QVariant();
}

Qt::ItemFlags ResultModel::flags(const QModelIndex& index) const {
    if (!index.isValid()) {
        return Qt::NoItemFlags;
    }
    // Selectable + enabled, but NOT editable: the grid is read-only; edits go
    // through re-issued SQL, never through the model.
    return Qt::ItemIsSelectable | Qt::ItemIsEnabled;
}

bool ResultModel::canFetchMore(const QModelIndex& parent) const {
    if (parent.isValid() || !query_) {
        return false;
    }
    // More to reveal, or the feeder is still running so more may land.
    return exposedRows_ < loadedRows_ || status_.streaming();
}

void ResultModel::fetchMore(const QModelIndex& parent) {
    if (parent.isValid() || !query_) {
        return;
    }
    refreshStatus();
    revealMore();
}

void ResultModel::onProgressTick() {
    // Clear the coalescing latch first, so a callback that arrives while we work
    // queues the NEXT tick rather than being dropped.
    tickQueued_.store(false);
    if (!query_) {
        return;
    }
    refreshStatus();
    revealMore();

    // Cells that drew as skeletons before their page landed must repaint now
    // that more rows are loaded. Emitting dataChanged over the whole exposed
    // range is O(1) to signal — the view intersects it with the viewport and
    // repaints only what is on screen, exactly like the macOS "reload visible
    // rows only" pass.
    if (exposedRows_ > 0 && columnCount_ > 0) {
        emit dataChanged(index(0, 0),
                         index(static_cast<int>(exposedRows_) - 1, columnCount_ - 1));
    }
}

void ResultModel::refreshStatus() {
    if (!query_) {
        return;
    }
    dg::QueryStatus next;
    try {
        next = dg::QueryStatus::parse(QString::fromStdString(query_->statusJson()));
    } catch (const dg::Error& e) {
        next.state = dg::QueryState::Failed;
        next.error = QString::fromUtf8(e.what());
    }

    // Columns only ever APPEND on the right — an existing column is never moved
    // or renamed by a schema delta (columns jumping mid-scroll is the failure
    // mode). If the count grew, tell the view about the new columns.
    const int newColumnCount = static_cast<int>(next.columns.size());
    if (newColumnCount > columnCount_) {
        beginInsertColumns(QModelIndex(), columnCount_, newColumnCount - 1);
        for (int i = columnCount_; i < newColumnCount; ++i) {
            columnNames_ << next.columns[i].name;
            columnTypes_ << next.columns[i].type;
            columnRightAligned_ << numericType(next.columns[i].type);
        }
        columnCount_ = newColumnCount;
        endInsertColumns();
        // Every resident window predates the wider schema and only carries the
        // OLD column count. Asking such a window for a new column would index
        // row*oldCols+col into its flat cell array — aliasing another row's
        // cell, not erroring. Drop them all; they re-fetch with the new width.
        if (pager_) {
            pager_->invalidateAll();
        }
    }

    // Streaming pages may have been short; drop only those so they re-fetch.
    if (pager_) {
        pager_->invalidatePartialPages();
    }

    loadedRows_ = next.rowsLoaded;
    status_ = next;
    emit statusChanged(status_);
}

void ResultModel::revealMore() {
    // Terminal => announce the whole remainder so the scrollbar is honest even
    // without scrolling. Streaming => reveal one bounded batch and let the view
    // pull the rest through repeated fetchMore() as it scrolls.
    const std::uint64_t target =
        status_.streaming()
            ? std::min(loadedRows_, exposedRows_ + kFetchBatch)
            : loadedRows_;
    revealTo(target);
}

void ResultModel::revealTo(std::uint64_t target) {
    // rows_loaded is u64 on the wire but a QModelIndex row is an int: clamp the
    // exposure ceiling so the beginInsertRows arithmetic below can never
    // overflow. (Every cast of exposedRows_ elsewhere relies on this clamp.)
    constexpr std::uint64_t kMaxRows =
        static_cast<std::uint64_t>(std::numeric_limits<int>::max());
    target = std::min(target, kMaxRows);
    if (target <= exposedRows_) {
        return;
    }
    const int first = static_cast<int>(exposedRows_);
    const int last = static_cast<int>(target) - 1;
    beginInsertRows(QModelIndex(), first, last);
    exposedRows_ = target;
    endInsertRows();
}

QString ResultModel::cancel() {
    if (!query_) {
        return QString();
    }
    if (auto outcome = query_->cancel()) {
        return QString::fromStdString(*outcome);
    }
    return QString();
}

QString ResultModel::cellDetailJson(int row, int column) const {
    if (!pager_ || row < 0 || column < 0 ||
        static_cast<std::uint64_t>(row) >= exposedRows_ || column >= columnCount_) {
        return QString();
    }
    const dg::RowWindow* window = nullptr;
    try {
        window = pager_->window(static_cast<std::uint64_t>(row));
    } catch (const dg::Error&) {
        return QString();
    }
    if (window == nullptr ||
        static_cast<std::uint32_t>(column) >= window->columns()) {
        // Same out-of-range hazard as data(): a stale narrow window would hand
        // back the WRONG cell, never an error. Refuse instead.
        return QString();
    }
    if (auto detail = window->cellDetailJson(static_cast<std::uint64_t>(row),
                                             static_cast<std::uint32_t>(column))) {
        return QString::fromStdString(*detail);
    }
    return QString();
}

std::optional<dg::CellKind> ResultModel::cellKind(int row, int column) const {
    if (!pager_ || row < 0 || column < 0 ||
        static_cast<std::uint64_t>(row) >= exposedRows_ || column >= columnCount_) {
        return std::nullopt;
    }
    const dg::RowWindow* window = nullptr;
    try {
        window = pager_->window(static_cast<std::uint64_t>(row));
    } catch (const dg::Error&) {
        return std::nullopt;
    }
    if (window == nullptr || window->pending() ||
        static_cast<std::uint32_t>(column) >= window->columns()) {
        return std::nullopt;
    }
    return window->kind(static_cast<std::uint64_t>(row),
                        static_cast<std::uint32_t>(column));
}

QString ResultModel::envelopeJson(int row) const {
    if (!pager_ || row < 0 || static_cast<std::uint64_t>(row) >= exposedRows_) {
        return QString();
    }
    const dg::RowWindow* window = nullptr;
    try {
        window = pager_->window(static_cast<std::uint64_t>(row));
    } catch (const dg::Error&) {
        return QString();
    }
    if (window == nullptr || window->pending()) {
        return QString();
    }
    if (auto envelope = window->envelopeJson(static_cast<std::uint64_t>(row))) {
        return QString::fromStdString(*envelope);
    }
    return QString();
}

bool ResultModel::numericType(const QString& type) const {
    const QString t = type.toLower();
    static const char* const kNumeric[] = {
        "int",    "integer", "bigint", "smallint", "tinyint", "serial",
        "float",  "double",  "real",   "decimal",  "numeric", "number",
        "long",   "short",   "byte",   "money",
    };
    for (const char* n : kNumeric) {
        if (t.contains(QLatin1String(n))) {
            return true;
        }
    }
    return false;
}
