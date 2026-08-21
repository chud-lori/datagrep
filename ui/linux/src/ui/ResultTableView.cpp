#include "ResultTableView.hpp"

#include "RowNumberHeader.hpp"
#include "model/ResultModel.hpp"

#include <QContextMenuEvent>
#include <QMenu>

#include <QAbstractItemModel>
#include <QApplication>
#include <QClipboard>
#include <QHeaderView>
#include <QItemSelectionModel>
#include <QMap>
#include <QModelIndexList>
#include <QShortcut>
#include <QString>
#include <QStringList>

ResultTableView::ResultTableView(QWidget* parent) : QTableView(parent) {
    rowHeader_ = new RowNumberHeader(this);
    setVerticalHeader(rowHeader_);

    setSelectionMode(QAbstractItemView::ExtendedSelection);
    setSelectionBehavior(QAbstractItemView::SelectItems);
    setAlternatingRowColors(true);
    setShowGrid(true);
    setWordWrap(false);
    setCornerButtonEnabled(true);  // corner "select all", like the macOS gutter head
    // Double-click is the universal "edit this cell" gesture. Whether a cell
    // actually edits is the MODEL's answer (flags() grants ItemIsEditable only
    // on a result the engine said is editable), so on everything else these
    // triggers are inert and the grid stays read-only.
    setEditTriggers(QAbstractItemView::DoubleClicked |
                    QAbstractItemView::EditKeyPressed);
    setHorizontalScrollMode(QAbstractItemView::ScrollPerPixel);
    setVerticalScrollMode(QAbstractItemView::ScrollPerPixel);

    // Uniform, fixed row height keeps geometry arithmetic — the model can report
    // millions of rows and the view still lays out in O(viewport).
    verticalHeader()->setSectionResizeMode(QHeaderView::Fixed);
    verticalHeader()->setDefaultSectionSize(fontMetrics().height() + 8);
    horizontalHeader()->setStretchLastSection(false);
    horizontalHeader()->setSectionResizeMode(QHeaderView::Interactive);

    // Ctrl+C / Cmd+C -> copySelection. Deliberately NOT the default view copy
    // (there is none); this is the single copy path, and it only ever touches
    // selectedIndexes().
    auto* copyShortcut = new QShortcut(QKeySequence::Copy, this);
    copyShortcut->setContext(Qt::WidgetWithChildrenShortcut);
    connect(copyShortcut, &QShortcut::activated, this,
            &ResultTableView::copySelection);
}

void ResultTableView::setModel(QAbstractItemModel* model) {
    if (model == this->model()) {
        return;  // re-setting the same model must not duplicate connections
    }
    // Drop the previous model's connections first: they capture `this` and
    // would otherwise keep firing after the model is swapped — dereferencing
    // model() (nullptr after setModel(nullptr)) or resizing the gutter from a
    // model this view no longer shows.
    if (QAbstractItemModel* old = this->model(); old != nullptr) {
        disconnect(old, nullptr, this, nullptr);
    }
    QTableView::setModel(model);
    if (model != nullptr) {
        connect(model, &QAbstractItemModel::modelReset, this, [this]() {
            if (this->model() != nullptr) {
                rowHeader_->updateWidthForRowCount(this->model()->rowCount());
            }
        });
        connect(model, &QAbstractItemModel::rowsInserted, this,
                [this](const QModelIndex&, int, int) {
                    if (this->model() != nullptr) {
                        rowHeader_->updateWidthForRowCount(this->model()->rowCount());
                    }
                });
        rowHeader_->updateWidthForRowCount(model->rowCount());
    }
}

void ResultTableView::copySelection() const {
    if (model() == nullptr || selectionModel() == nullptr) {
        return;
    }
    // ONLY model cells. A QHeaderView row number is not a QModelIndex, so it can
    // never be in this list — that is the whole copy-safety guarantee.
    const QModelIndexList indexes = selectionModel()->selectedIndexes();
    if (indexes.isEmpty()) {
        return;
    }

    // Group cell text by (row -> column -> text) so the TSV comes out in visual
    // order regardless of selection order.
    QMap<int, QMap<int, QString>> grid;
    for (const QModelIndex& idx : indexes) {
        grid[idx.row()][idx.column()] = idx.data(Qt::DisplayRole).toString();
    }

    QStringList lines;
    lines.reserve(grid.size());
    for (auto rowIt = grid.constBegin(); rowIt != grid.constEnd(); ++rowIt) {
        const QMap<int, QString>& cols = rowIt.value();
        QStringList cells;
        cells.reserve(cols.size());
        for (auto colIt = cols.constBegin(); colIt != cols.constEnd(); ++colIt) {
            cells << colIt.value();
        }
        lines << cells.join(QLatin1Char('\t'));
    }

    QApplication::clipboard()->setText(lines.join(QLatin1Char('\n')));
}

void ResultTableView::contextMenuEvent(QContextMenuEvent* event) {
    auto* result = qobject_cast<ResultModel*>(model());
    const QModelIndex idx = indexAt(event->pos());
    if (result == nullptr || !idx.isValid() || !result->editable()) {
        QTableView::contextMenuEvent(event);
        return;
    }
    const int row = idx.row();
    QMenu menu(this);
    if (result->cellEditable(row, idx.column())) {
        menu.addAction(QStringLiteral("Edit Cell"),
                       [this, idx]() { edit(idx); });
    }
    const bool deleted = result->rowIsDeleted(row);
    if (deleted) {
        menu.addAction(QStringLiteral("Keep This Document"),
                       [result, row]() { result->discardStagedRow(row); });
    } else {
        menu.addAction(QStringLiteral("Delete Document"),
                       [result, row]() { result->stageDeleteRow(row); });
    }
    if (result->rowIsStaged(row) && !deleted) {
        menu.addAction(QStringLiteral("Discard Staged Changes"),
                       [result, row]() { result->discardStagedRow(row); });
    }
    if (!menu.isEmpty()) {
        menu.exec(event->globalPos());
    }
}
