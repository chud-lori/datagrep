#include "ResultModel.hpp"

#include <QBrush>
#include <QColor>
#include <QFont>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QMetaObject>
#include <QPalette>

#include <algorithm>
#include <limits>

ResultModel::ResultModel(QObject* parent) : QAbstractTableModel(parent) {}

ResultModel::~ResultModel() {
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
    editable_.reset();
    namesWindow_ = nullptr;
    namesCache_.clear();
    if (edits_ != nullptr) {
        edits_->discardAll();
    }
    if (query_) {
        pager_ = std::make_unique<dg::RowPager>(*query_);
        query_->onProgress([this]() {
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
    editable_.reset();
    namesWindow_ = nullptr;
    namesCache_.clear();
    if (edits_ != nullptr) {
        edits_->discardAll();
    }
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

    const dg::RowWindow* window = nullptr;
    try {
        window = pager_->window(absRow);
    } catch (const dg::Error&) {
        window = nullptr;
    }

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

    const dg::StagedDocument* stagedDoc =
        (editable_ && edits_ != nullptr) ? edits_->documentAtRow(row) : nullptr;
    std::optional<dg::MutationValue> stagedValue;
    if (stagedDoc != nullptr) {
        const QString field = fieldName(window, col);
        if (!field.isEmpty()) {
            stagedValue = stagedDoc->valueOf(field);
        }
    }

    switch (role) {
        case Qt::EditRole:
            if (stagedValue) {
                return stagedValue->display();
            }
            switch (kind) {
                case dg::CellKind::Null:
                    return QStringLiteral("NULL");
                case dg::CellKind::Absent:
                    return QString();
                default:
                    return QString::fromStdString(window->cellText(absRow, absCol));
            }
        case Qt::FontRole:
            if (stagedDoc != nullptr && stagedDoc->isDelete) {
                QFont f;
                f.setStrikeOut(true);
                return f;
            }
            return QVariant();
        case Qt::BackgroundRole:
            if (stagedDoc != nullptr) {
                if (stagedDoc->isDelete) {
                    return QBrush(QColor(220, 60, 60, 36));
                }
                if (stagedDoc->isConflicted()) {
                    return stagedValue ? QVariant(QBrush(QColor(235, 140, 20, 70)))
                                       : QVariant();
                }
                if (stagedDoc->state.isDone()) {
                    return stagedValue ? QVariant(QBrush(QColor(60, 170, 90, 46)))
                                       : QVariant();
                }
                if (stagedValue) {
                    return QBrush(QColor(235, 170, 40, 46));
                }
            }
            return QVariant();
        case Qt::DisplayRole: {
            if (stagedValue) {
                return stagedValue->display();
            }
            switch (kind) {
                case dg::CellKind::Null:
                    return QStringLiteral("NULL");
                case dg::CellKind::Absent:
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
            // A staged value is data, however chrome the loaded cell was.
            if (stagedValue) {
                return QVariant();
            }
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
    Qt::ItemFlags f = Qt::ItemIsSelectable | Qt::ItemIsEnabled;
    if (editable_ && cellEditable(index.row(), index.column())) {
        f |= Qt::ItemIsEditable;
    }
    return f;
}

bool ResultModel::cellEditable(int row, int column) const {
    if (!editable_ || !pager_ || row < 0 ||
        static_cast<std::uint64_t>(row) >= exposedRows_ || column < 0 ||
        column >= columnCount_) {
        return false;
    }
    const dg::RowWindow* window = nullptr;
    try {
        window = pager_->window(static_cast<std::uint64_t>(row));
    } catch (const dg::Error&) {
        return false;
    }
    if (window == nullptr ||
        static_cast<std::uint32_t>(column) >= window->columns()) {
        return false;
    }
    // A document or an array is edited in the inspector, not in a grid cell.
    return window->kind(static_cast<std::uint64_t>(row),
                        static_cast<std::uint32_t>(column)) != dg::CellKind::Nested;
}

bool ResultModel::setData(const QModelIndex& index, const QVariant& value,
                          int role) {
    if (role != Qt::EditRole || !index.isValid() || !editable_ ||
        edits_ == nullptr || !pager_) {
        return false;
    }
    const int row = index.row();
    const int col = index.column();
    const dg::RowWindow* window = nullptr;
    try {
        window = pager_->window(static_cast<std::uint64_t>(row));
    } catch (const dg::Error&) {
        return false;
    }
    if (window == nullptr || static_cast<std::uint32_t>(col) >= window->columns()) {
        return false;
    }
    namesWindow_ = nullptr;
    const QString field = fieldName(window, col);
    if (field.isEmpty()) {
        emit editRefused(QStringLiteral(
            "this column is not one of the fields the row was read under"));
        return false;
    }
    const auto loaded = loadedValue(window, row, col);
    const QString typed = value.toString();
    if (loaded && loaded->display() == typed) {
        edits_->unstage(row, field);
        refreshStagedRows({row});
        return true;
    }
    dg::MutationValue coerced;
    QString whyNot;
    if (!dg::MutationValue::typedLike(typed, loaded, &coerced, &whyNot)) {
        emit editRefused(QStringLiteral("`%1`: %2").arg(field, whyNot));
        return false;
    }
    dg::EditableResult::Address address;
    if (!addressRow(row, window, &address)) {
        return false;
    }
    edits_->stage(address.id, row, address.key, address.expect, field, coerced,
                  loaded);
    refreshStagedRows({row});
    return true;
}

bool ResultModel::rowIsStaged(int row) const {
    return edits_ != nullptr && edits_->documentAtRow(row) != nullptr;
}

bool ResultModel::rowIsDeleted(int row) const {
    return edits_ != nullptr && edits_->isDeleted(row);
}

void ResultModel::stageDeleteRow(int row) {
    if (!editable_ || edits_ == nullptr || !pager_ || row < 0 ||
        static_cast<std::uint64_t>(row) >= exposedRows_) {
        return;
    }
    const dg::RowWindow* window = nullptr;
    try {
        window = pager_->window(static_cast<std::uint64_t>(row));
    } catch (const dg::Error&) {
        return;
    }
    if (window == nullptr) {
        return;
    }
    dg::EditableResult::Address address;
    if (!addressRow(row, window, &address)) {
        return;
    }
    edits_->stageDelete(address.id, row, address.key, address.expect);
    refreshStagedRows({row});
}

void ResultModel::discardStagedRow(int row) {
    if (edits_ == nullptr || edits_->documentAtRow(row) == nullptr) {
        return;
    }
    edits_->discardRow(row);
    refreshStagedRows({row});
}

void ResultModel::refreshStagedRows(const QVector<int>& rows) {
    if (columnCount_ == 0) {
        return;
    }
    for (int row : rows) {
        if (row >= 0 && static_cast<std::uint64_t>(row) < exposedRows_) {
            emit dataChanged(index(row, 0), index(row, columnCount_ - 1));
        }
    }
}

QString ResultModel::fieldName(const dg::RowWindow* window, int col) const {
    if (window == nullptr || col < 0) {
        return QString();
    }
    if (window != namesWindow_) {
        namesCache_.clear();
        if (auto json = window->columnNamesJson()) {
            const QJsonDocument doc =
                QJsonDocument::fromJson(QByteArray::fromStdString(*json));
            for (const QJsonValue& v : doc.array()) {
                namesCache_ << v.toString();
            }
        }
        namesWindow_ = window;
    }
    return col < namesCache_.size() ? namesCache_.at(col) : QString();
}

std::optional<dg::MutationValue> ResultModel::loadedValue(
    const dg::RowWindow* window, int row, int col) const {
    const auto absRow = static_cast<std::uint64_t>(row);
    const auto absCol = static_cast<std::uint32_t>(col);
    const dg::CellKind kind = window->kind(absRow, absCol);
    if (kind == dg::CellKind::Nested || kind == dg::CellKind::Absent) {
        return std::nullopt;
    }
    const auto detail = window->cellDetailJson(absRow, absCol);
    if (!detail) {
        return std::nullopt;
    }
    return dg::MutationValue::decodeFragment(QString::fromStdString(*detail));
}

bool ResultModel::addressRow(int row, const dg::RowWindow* window,
                             dg::EditableResult::Address* out) {
    const auto envelope = window->envelopeJson(static_cast<std::uint64_t>(row));
    if (!envelope) {
        emit editRefused(QStringLiteral(
            "this row carries no document envelope, so datagrep cannot tell "
            "which document it is"));
        return false;
    }
    const QJsonDocument doc =
        QJsonDocument::fromJson(QByteArray::fromStdString(*envelope));
    QString whyNot;
    if (!editable_->address(doc.object(), out, &whyNot)) {
        emit editRefused(whyNot);
        return false;
    }
    return true;
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
    tickQueued_.store(false);
    if (!query_) {
        return;
    }
    refreshStatus();
    revealMore();

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
    editable_ = allowsEditing_ ? next.editable : std::nullopt;
    namesWindow_ = nullptr;
    namesCache_.clear();
    emit statusChanged(status_);
}

void ResultModel::revealMore() {
    const std::uint64_t target =
        status_.streaming()
            ? std::min(loadedRows_, exposedRows_ + kFetchBatch)
            : loadedRows_;
    revealTo(target);
}

void ResultModel::revealTo(std::uint64_t target) {
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
