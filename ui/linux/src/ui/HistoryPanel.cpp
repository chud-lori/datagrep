#include "HistoryPanel.hpp"

#include "ui/EngineIcon.hpp"

#include <QBrush>
#include <QClipboard>
#include <QColor>
#include <QComboBox>
#include <QDialog>
#include <QDialogButtonBox>
#include <QFont>
#include <QFontDatabase>
#include <QFormLayout>
#include <QGuiApplication>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QLabel>
#include <QLineEdit>
#include <QLocale>
#include <QMenu>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QShortcut>
#include <QSpinBox>
#include <QSplitter>
#include <QToolButton>
#include <QTreeWidget>
#include <QTreeWidgetItem>
#include <QVBoxLayout>

namespace {

constexpr int kIdRole = Qt::UserRole;

enum Columns {
    ColStatement = 0,
    ColOutcome,
    ColConnection,
    ColTime,
    ColDuration,
    ColRows,
    ColRuns,
    ColCount,
};

QColor outcomeColor(dg::QueryOutcome o) {
    switch (o) {
        case dg::QueryOutcome::Ok: return QColor(0x1E, 0x8E, 0x3E);
        case dg::QueryOutcome::Error: return QColor(0xC0, 0x39, 0x2B);
        case dg::QueryOutcome::Cancelled: return QColor(0xB9, 0x77, 0x0E);
    }
    return QColor();
}

QTreeWidgetItem* placeholderItem(QTreeWidget* tree, const QString& text) {
    auto* item = new QTreeWidgetItem(tree);
    item->setText(0, text);
    item->setFirstColumnSpanned(true);
    item->setFlags(Qt::NoItemFlags);
    return item;
}

}  // namespace

HistoryPanel::HistoryPanel(QueryHistoryStore* store, QWidget* parent)
    : QWidget(parent), store_(store) {
    auto* layout = new QVBoxLayout(this);
    layout->setContentsMargins(6, 6, 6, 6);
    layout->setSpacing(6);

    // --- filter bar ---------------------------------------------------------
    search_ = new QLineEdit(this);
    search_->setPlaceholderText(QStringLiteral("Search SQL and error text"));
    search_->setClearButtonEnabled(true);
    connect(search_, &QLineEdit::textChanged, this, &HistoryPanel::refresh);

    connectionFilter_ = new QComboBox(this);
    connectionFilter_->addItem(QStringLiteral("All connections"), QString());
    connectionFilter_->setToolTip(
        QStringLiteral("Filter by the connection a statement was run against"));
    connect(connectionFilter_, &QComboBox::currentIndexChanged, this,
            &HistoryPanel::refresh);

    rangeFilter_ = new QComboBox(this);
    rangeFilter_->addItem(QStringLiteral("All dates"),
                          static_cast<int>(dg::HistoryDateRange::All));
    rangeFilter_->addItem(QStringLiteral("Today"),
                          static_cast<int>(dg::HistoryDateRange::Day));
    rangeFilter_->addItem(QStringLiteral("Past week"),
                          static_cast<int>(dg::HistoryDateRange::Week));
    rangeFilter_->addItem(QStringLiteral("Past month"),
                          static_cast<int>(dg::HistoryDateRange::Month));
    rangeFilter_->setToolTip(QStringLiteral("Filter by when the statement ran"));
    connect(rangeFilter_, &QComboBox::currentIndexChanged, this,
            &HistoryPanel::refresh);

    outcomeFilter_ = new QComboBox(this);
    outcomeFilter_->addItem(QStringLiteral("Any outcome"), -1);
    outcomeFilter_->addItem(QStringLiteral("ok"),
                            static_cast<int>(dg::QueryOutcome::Ok));
    outcomeFilter_->addItem(QStringLiteral("failed"),
                            static_cast<int>(dg::QueryOutcome::Error));
    outcomeFilter_->addItem(QStringLiteral("cancelled"),
                            static_cast<int>(dg::QueryOutcome::Cancelled));
    outcomeFilter_->setToolTip(
        QStringLiteral("Filter by outcome — ok, failed or cancelled"));
    connect(outcomeFilter_, &QComboBox::currentIndexChanged, this,
            &HistoryPanel::refresh);

    clearFiltersButton_ = new QPushButton(QStringLiteral("Clear"), this);
    clearFiltersButton_->setToolTip(QStringLiteral("Remove every filter"));
    clearFiltersButton_->hide();
    connect(clearFiltersButton_, &QPushButton::clicked, this, [this]() {
        search_->clear();
        connectionFilter_->setCurrentIndex(0);
        rangeFilter_->setCurrentIndex(0);
        outcomeFilter_->setCurrentIndex(0);
    });

    countLabel_ = new QLabel(this);

    auto* filterRow = new QHBoxLayout();
    filterRow->setSpacing(6);
    filterRow->addWidget(search_, 1);
    filterRow->addWidget(connectionFilter_);
    filterRow->addWidget(rangeFilter_);
    filterRow->addWidget(outcomeFilter_);
    filterRow->addWidget(clearFiltersButton_);
    filterRow->addWidget(countLabel_);
    layout->addLayout(filterRow);

    // --- the list, grouped by day -------------------------------------------
    list_ = new QTreeWidget(this);
    list_->setColumnCount(ColCount);
    list_->setHeaderLabels({QStringLiteral("Statement"), QStringLiteral("Outcome"),
                            QStringLiteral("Connection"), QStringLiteral("Time"),
                            QStringLiteral("Duration"), QStringLiteral("Rows"),
                            QStringLiteral("Runs")});
    list_->setRootIsDecorated(false);
    list_->setItemsExpandable(false);
    list_->setAllColumnsShowFocus(true);
    list_->setSelectionMode(QAbstractItemView::SingleSelection);
    list_->setUniformRowHeights(true);
    list_->header()->setStretchLastSection(false);
    list_->header()->setSectionResizeMode(ColStatement, QHeaderView::Stretch);
    for (int c = ColOutcome; c < ColCount; ++c) {
        list_->header()->setSectionResizeMode(c, QHeaderView::ResizeToContents);
    }
    list_->setContextMenuPolicy(Qt::CustomContextMenu);
    connect(list_, &QTreeWidget::itemSelectionChanged, this,
            &HistoryPanel::onSelectionChanged);
    connect(list_, &QTreeWidget::itemDoubleClicked, this,
            [this](QTreeWidgetItem*, int) { openSelected(); });
    connect(list_, &QWidget::customContextMenuRequested, this,
            [this](const QPoint& pos) {
                if (!selectedEntry().has_value()) {
                    return;
                }
                QMenu menu(this);
                menu.addAction(QStringLiteral("Copy SQL"), this,
                               &HistoryPanel::copySelected);
                menu.addAction(QStringLiteral("Open in Editor"), this,
                               &HistoryPanel::openSelected);
                menu.addAction(QStringLiteral("Run Again"), this,
                               &HistoryPanel::rerunSelected);
                menu.addSeparator();
                menu.addAction(QStringLiteral("Remove from History"), this,
                               &HistoryPanel::removeSelected);
                menu.exec(list_->viewport()->mapToGlobal(pos));
            });
    auto* deleteShortcut = new QShortcut(QKeySequence::Delete, list_);
    deleteShortcut->setContext(Qt::WidgetWithChildrenShortcut);
    connect(deleteShortcut, &QShortcut::activated, this,
            &HistoryPanel::removeSelected);

    // --- the detail strip: full statement, its error, and the actions --------
    detail_ = new QWidget(this);
    auto* detailLayout = new QVBoxLayout(detail_);
    detailLayout->setContentsMargins(0, 0, 0, 0);
    detailLayout->setSpacing(4);

    detailSummary_ = new QLabel(detail_);
    auto* copyButton = new QPushButton(QStringLiteral("Copy SQL"), detail_);
    connect(copyButton, &QPushButton::clicked, this, &HistoryPanel::copySelected);
    auto* openButton = new QPushButton(QStringLiteral("Open in Editor"), detail_);
    openButton->setToolTip(QStringLiteral(
        "Put this statement in the editor, on the connection it ran against"));
    connect(openButton, &QPushButton::clicked, this, &HistoryPanel::openSelected);
    auto* rerunButton = new QPushButton(QStringLiteral("Run Again"), detail_);
    rerunButton->setToolTip(QStringLiteral("Run this statement again now"));
    connect(rerunButton, &QPushButton::clicked, this, &HistoryPanel::rerunSelected);
    auto* removeButton = new QPushButton(QStringLiteral("Remove"), detail_);
    connect(removeButton, &QPushButton::clicked, this,
            &HistoryPanel::removeSelected);

    auto* detailHeader = new QHBoxLayout();
    detailHeader->setSpacing(6);
    detailHeader->addWidget(detailSummary_, 1);
    detailHeader->addWidget(copyButton);
    detailHeader->addWidget(openButton);
    detailHeader->addWidget(rerunButton);
    detailHeader->addWidget(removeButton);
    detailLayout->addLayout(detailHeader);

    const QFont mono = QFontDatabase::systemFont(QFontDatabase::FixedFont);
    detailSql_ = new QPlainTextEdit(detail_);
    detailSql_->setReadOnly(true);
    detailSql_->setFont(mono);
    detailSql_->setMaximumHeight(110);
    detailLayout->addWidget(detailSql_);

    detailError_ = new QPlainTextEdit(detail_);
    detailError_->setReadOnly(true);
    detailError_->setFont(mono);
    detailError_->setMaximumHeight(64);
    detailError_->setStyleSheet(
        QStringLiteral("QPlainTextEdit { color: #C0392B; }"));
    detailError_->hide();
    detailLayout->addWidget(detailError_);

    detail_->hide();

    auto* splitter = new QSplitter(Qt::Vertical, this);
    splitter->addWidget(list_);
    splitter->addWidget(detail_);
    splitter->setStretchFactor(0, 1);
    splitter->setStretchFactor(1, 0);
    layout->addWidget(splitter, 1);

    // --- footer: retention stated, retention editable, and Clear… ------------
    retentionLabel_ = new QLabel(this);
    retentionButton_ = new QPushButton(QStringLiteral("Retention…"), this);
    retentionButton_->setToolTip(QStringLiteral(
        "Choose how many queries, and how many days, of history to keep"));
    connect(retentionButton_, &QPushButton::clicked, this,
            &HistoryPanel::editRetention);

    clearButton_ = new QToolButton(this);
    clearButton_->setText(QStringLiteral("Clear…"));
    clearButton_->setPopupMode(QToolButton::InstantPopup);
    auto* clearMenu = new QMenu(clearButton_);
    connect(clearMenu, &QMenu::aboutToShow, this, [this, clearMenu]() {
        clearMenu->clear();
        const QString c = connectionFilter_->currentData().toString();
        if (!c.isEmpty()) {
            clearMenu->addAction(
                QStringLiteral("Clear History for ‘%1’").arg(c), this, [this, c]() {
                    selectedId_.clear();
                    store_->clear(c);
                    emit statusMessage(QStringLiteral("history cleared for %1").arg(c));
                });
        }
        clearMenu->addAction(QStringLiteral("Clear All History"), this, [this]() {
            selectedId_.clear();
            store_->clear();
            emit statusMessage(QStringLiteral("query history cleared"));
        });
    });
    clearButton_->setMenu(clearMenu);

    auto* footer = new QHBoxLayout();
    footer->setSpacing(6);
    footer->addWidget(retentionLabel_, 1);
    footer->addWidget(retentionButton_);
    footer->addWidget(clearButton_);
    layout->addLayout(footer);

    connect(store_, &QueryHistoryStore::changed, this, &HistoryPanel::refresh);
    refresh();
}

dg::HistoryFilter HistoryPanel::currentFilter() const {
    dg::HistoryFilter f;
    f.text = search_->text();
    f.connection = connectionFilter_->currentData().toString();
    f.range = static_cast<dg::HistoryDateRange>(rangeFilter_->currentData().toInt());
    const int outcome = outcomeFilter_->currentData().toInt();
    if (outcome >= 0) {
        f.outcome = static_cast<dg::QueryOutcome>(outcome);
    }
    return f;
}

std::optional<dg::QueryHistoryEntry> HistoryPanel::selectedEntry() const {
    const auto items = list_->selectedItems();
    if (items.isEmpty()) {
        return std::nullopt;
    }
    const QString id = items.first()->data(0, kIdRole).toString();
    if (id.isEmpty()) {
        return std::nullopt;
    }
    for (const dg::QueryHistoryEntry& e : store_->entries()) {
        if (e.id == id) {
            return e;
        }
    }
    return std::nullopt;
}

void HistoryPanel::rebuildConnectionCombo(const QStringList& names) {
    const QString current = connectionFilter_->currentData().toString();
    // Rebuilding must not fire refresh() recursively; refreshing_ guards it.
    connectionFilter_->clear();
    connectionFilter_->addItem(QStringLiteral("All connections"), QString());
    int selectIndex = 0;
    for (const QString& name : names) {
        connectionFilter_->addItem(name, name);
        if (name == current) {
            selectIndex = connectionFilter_->count() - 1;
        }
    }
    connectionFilter_->setCurrentIndex(selectIndex);
}

void HistoryPanel::refresh() {
    if (refreshing_) {
        return;
    }
    refreshing_ = true;

    const auto& all = store_->entries();
    rebuildConnectionCombo(QueryHistoryStore::connections(all));
    const dg::HistoryFilter f = currentFilter();
    const auto shown = QueryHistoryStore::filter(all, f);

    if (all.isEmpty()) {
        countLabel_->setText(QStringLiteral("nothing has been run yet"));
    } else if (!f.isEmpty()) {
        countLabel_->setText(QStringLiteral("%1 of %2 queries")
                                 .arg(QLocale().toString(shown.size()))
                                 .arg(QLocale().toString(all.size())));
    } else {
        countLabel_->setText(all.size() == 1
                                 ? QStringLiteral("1 query")
                                 : QStringLiteral("%1 queries")
                                       .arg(QLocale().toString(all.size())));
    }
    clearFiltersButton_->setVisible(!f.isEmpty());
    clearButton_->setEnabled(!all.isEmpty());
    retentionLabel_->setText(store_->retention().summary());

    const QFont mono = QFontDatabase::systemFont(QFontDatabase::FixedFont);
    list_->clear();
    QTreeWidgetItem* toSelect = nullptr;
    const auto days = QueryHistoryStore::group(shown);
    for (const dg::HistoryDay& day : days) {
        auto* section = new QTreeWidgetItem(list_);
        section->setText(0, day.title);
        section->setFirstColumnSpanned(true);
        QFont bold = section->font(0);
        bold.setBold(true);
        section->setFont(0, bold);
        section->setFlags(Qt::ItemIsEnabled);
        for (const dg::QueryHistoryEntry& e : day.entries) {
            auto* item = new QTreeWidgetItem(section);
            item->setText(ColStatement, e.oneLine());
            item->setFont(ColStatement, mono);
            item->setToolTip(ColStatement, e.sql);
            item->setText(ColOutcome, dg::outcomeLabel(e.outcome));
            item->setForeground(ColOutcome, outcomeColor(e.outcome));
            item->setText(ColConnection, e.connection.isEmpty()
                                             ? QStringLiteral("no connection")
                                             : e.connection);
            if (!e.engine.isEmpty()) {
                item->setIcon(ColConnection, dg::engineIcon(e.engine));
                item->setToolTip(ColConnection, dg::engineDisplayName(e.engine));
            }
            item->setText(ColTime, dg::historyformat::time(e.startedAt()));
            item->setText(ColDuration, dg::historyformat::duration(e.durationMs));
            if (e.outcome == dg::QueryOutcome::Ok) {
                item->setText(ColRows, dg::historyformat::rows(e.rowCount));
            }
            if (e.runCount > 1) {
                item->setText(ColRuns, QStringLiteral("×%1").arg(e.runCount));
                item->setToolTip(
                    ColRuns,
                    QStringLiteral("Run %1 times in quick succession — collapsed "
                                   "into one entry")
                        .arg(e.runCount));
            }
            item->setData(0, kIdRole, e.id);
            if (e.id == selectedId_) {
                toSelect = item;
            }
        }
        section->setExpanded(true);
    }
    if (all.isEmpty()) {
        placeholderItem(
            list_,
            QStringLiteral("Every statement datagrep runs is logged here "
                           "automatically — the SQL, the connection, how long it "
                           "took, and what came back."));
    } else if (shown.isEmpty()) {
        placeholderItem(list_,
                        QStringLiteral("No recorded query matches these filters."));
    }

    refreshing_ = false;
    if (toSelect != nullptr) {
        list_->setCurrentItem(toSelect);
    } else {
        selectedId_.clear();
        showDetail(std::nullopt);
    }
}

void HistoryPanel::onSelectionChanged() {
    if (refreshing_) {
        return;
    }
    const auto entry = selectedEntry();
    selectedId_ = entry.has_value() ? entry->id : QString();
    showDetail(entry);
}

void HistoryPanel::showDetail(const std::optional<dg::QueryHistoryEntry>& entry) {
    if (!entry.has_value()) {
        detail_->hide();
        return;
    }
    QStringList parts;
    if (!entry->connection.isEmpty()) {
        parts << (entry->engine.isEmpty()
                      ? entry->connection
                      : QStringLiteral("%1 · %2").arg(
                            entry->connection,
                            dg::engineDisplayName(entry->engine)));
    }
    parts << QStringLiteral("%1 %2").arg(
        dg::historyformat::dayTitle(entry->startedAt().date()),
        dg::historyformat::time(entry->startedAt()));
    parts << dg::historyformat::duration(entry->durationMs);
    if (entry->outcome == dg::QueryOutcome::Ok) {
        const QString rows = dg::historyformat::rows(entry->rowCount);
        if (!rows.isEmpty()) {
            parts << rows;
        }
    } else {
        parts << dg::outcomeLabel(entry->outcome);
    }
    if (entry->runCount > 1) {
        parts << QStringLiteral("run %1×").arg(entry->runCount);
    }
    detailSummary_->setText(parts.join(QStringLiteral("  ·  ")));
    detailSql_->setPlainText(entry->sql);
    detailError_->setVisible(!entry->error.isEmpty());
    detailError_->setPlainText(entry->error);
    detail_->show();
}

void HistoryPanel::copySelected() {
    const auto entry = selectedEntry();
    if (!entry.has_value()) {
        return;
    }
    QGuiApplication::clipboard()->setText(entry->sql);
    emit statusMessage(QStringLiteral("copied %1 characters of SQL")
                           .arg(entry->sql.size()));
}

void HistoryPanel::openSelected() {
    const auto entry = selectedEntry();
    if (!entry.has_value()) {
        return;
    }
    emit openInEditor(entry->sql, entry->connection);
}

void HistoryPanel::rerunSelected() {
    const auto entry = selectedEntry();
    if (!entry.has_value()) {
        return;
    }
    emit rerunRequested(entry->sql, entry->connection);
}

void HistoryPanel::removeSelected() {
    const auto entry = selectedEntry();
    if (!entry.has_value()) {
        return;
    }
    selectedId_.clear();
    store_->remove({entry->id});
}

void HistoryPanel::editRetention() {
    QDialog dialog(this);
    dialog.setWindowTitle(QStringLiteral("How much history to keep"));
    auto* layout = new QVBoxLayout(&dialog);

    auto* info = new QLabel(
        QStringLiteral("datagrep keeps whichever limit is reached first. Entries "
                       "are stored as one plain JSON-lines file per day in %1, so "
                       "nothing here is locked away.")
            .arg(store_->directory()),
        &dialog);
    info->setWordWrap(true);
    layout->addWidget(info);

    const dg::HistoryRetention current = store_->retention();
    auto* entriesSpin = new QSpinBox(&dialog);
    entriesSpin->setRange(100, 1000000);
    entriesSpin->setValue(current.maxEntries);
    entriesSpin->setGroupSeparatorShown(true);
    auto* daysSpin = new QSpinBox(&dialog);
    daysSpin->setRange(1, 3650);
    daysSpin->setValue(current.maxDays);

    auto* form = new QFormLayout();
    form->addRow(QStringLiteral("Entries (newest kept)"), entriesSpin);
    form->addRow(QStringLiteral("Days (older entries dropped)"), daysSpin);
    layout->addLayout(form);

    auto* buttons = new QDialogButtonBox(
        QDialogButtonBox::Ok | QDialogButtonBox::Cancel, &dialog);
    buttons->button(QDialogButtonBox::Ok)->setText(QStringLiteral("Apply"));
    auto* resetButton = buttons->addButton(QStringLiteral("Reset to defaults"),
                                           QDialogButtonBox::ResetRole);
    connect(resetButton, &QPushButton::clicked, &dialog,
            [entriesSpin, daysSpin]() {
                const dg::HistoryRetention d{};
                entriesSpin->setValue(d.maxEntries);
                daysSpin->setValue(d.maxDays);
            });
    connect(buttons, &QDialogButtonBox::accepted, &dialog, &QDialog::accept);
    connect(buttons, &QDialogButtonBox::rejected, &dialog, &QDialog::reject);
    layout->addWidget(buttons);

    if (dialog.exec() == QDialog::Accepted) {
        store_->setRetention(
            dg::HistoryRetention::clamped(entriesSpin->value(), daysSpin->value()));
    }
}
