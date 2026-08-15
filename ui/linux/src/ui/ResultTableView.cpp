#include "ResultTableView.hpp"

#include "RowNumberHeader.hpp"

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
    setEditTriggers(QAbstractItemView::NoEditTriggers);
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
    QTableView::setModel(model);
    if (model != nullptr) {
        connect(model, &QAbstractItemModel::modelReset, this, [this]() {
            rowHeader_->updateWidthForRowCount(this->model()->rowCount());
        });
        connect(model, &QAbstractItemModel::rowsInserted, this,
                [this](const QModelIndex&, int, int) {
                    rowHeader_->updateWidthForRowCount(this->model()->rowCount());
                });
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
