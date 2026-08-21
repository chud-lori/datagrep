#include "MainWindow.hpp"

#include "ffi/DatagrepFfi.hpp"
#include "model/ConnectionSafety.hpp"
#include "model/QueryHistory.hpp"
#include "model/ResultModel.hpp"
#include "ui/ConnectionDialog.hpp"
#include "ui/DetailPanel.hpp"
#include "ui/EditorTabs.hpp"
#include "ui/EngineIcon.hpp"
#include "ui/HistoryPanel.hpp"
#include "ui/ResultTableView.hpp"
#include "ui/SchemaTree.hpp"
#include "ui/SqlEditor.hpp"
#include "ui/StatusBar.hpp"

#include <QAction>
#include <QBrush>
#include <QColor>
#include <QDir>
#include <QDockWidget>
#include <QFont>
#include <QHBoxLayout>
#include <QIcon>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QKeySequence>
#include <QLabel>
#include <QListWidget>
#include <QMessageBox>
#include <QPushButton>
#include <QSize>
#include <QSplitter>
#include <QStandardPaths>
#include <QStatusBar>
#include <QTextCursor>
#include <QToolBar>
#include <QVBoxLayout>
#include <QWidget>

#include <optional>

namespace {

// The profiles store lives in the platform's per-user app data directory. The
// engine (datagrep_core_new) opens/creates the SQLite file at this path.
QString profilesDbPath() {
    const QString dir =
        QStandardPaths::writableLocation(QStandardPaths::AppDataLocation);
    QDir().mkpath(dir);
    return dir + QStringLiteral("/profiles.db");
}

// The @limit N block directive parsed from the statement being run. Mirrors
// DatagrepKit.SQLBlocks.directives: a line that is a `--` comment whose body
// starts with `@limit`. Used ONLY so the status bar can say "first N rows
// (@limit)" honestly when a result sits at the limit — it never bounds the query
// itself (the engine does that).
std::optional<std::uint64_t> parseLimitDirective(const QString& sql) {
    const QStringList lines = sql.split(QLatin1Char('\n'));
    for (const QString& raw : lines) {
        const QString line = raw.trimmed();
        if (!line.startsWith(QStringLiteral("--"))) {
            continue;
        }
        const QString body = line.mid(2).trimmed();
        if (!body.startsWith(QLatin1Char('@'))) {
            continue;
        }
        const QString rest = body.mid(1);
        const int sp = rest.indexOf(QLatin1Char(' '));
        const QString key = (sp >= 0 ? rest.left(sp) : rest).toLower();
        if (key != QStringLiteral("limit") || sp < 0) {
            continue;
        }
        bool ok = false;
        const qlonglong n = rest.mid(sp + 1).trimmed().toLongLong(&ok);
        if (ok && n >= 0) {
            return static_cast<std::uint64_t>(n);
        }
    }
    return std::nullopt;
}

// The `-- @connection NAME` block directive. Mirrors the macOS directive of the
// same name: text the user wrote outranks the tab's binding and the sidebar.
QString parseConnectionDirective(const QString& sql) {
    const QStringList lines = sql.split(QLatin1Char('\n'));
    for (const QString& raw : lines) {
        const QString line = raw.trimmed();
        if (!line.startsWith(QStringLiteral("--"))) {
            continue;
        }
        const QString body = line.mid(2).trimmed();
        if (!body.startsWith(QLatin1Char('@'))) {
            continue;
        }
        const QString rest = body.mid(1);
        const int sp = rest.indexOf(QLatin1Char(' '));
        if (sp < 0 || rest.left(sp).toLower() != QStringLiteral("connection")) {
            continue;
        }
        const QString value = rest.mid(sp + 1).trimmed();
        if (!value.isEmpty()) {
            return value;
        }
    }
    return QString();
}

}  // namespace

MainWindow::MainWindow(QWidget* parent) : QMainWindow(parent) {
    setWindowTitle(QStringLiteral("datagrep"));

    // --- engine ------------------------------------------------------------
    // A failure here is fatal to the session but must be reported honestly, not
    // swallowed. The window still constructs so the message is visible.
    try {
        core_ = std::make_unique<dg::Core>(profilesDbPath().toStdString());
    } catch (const dg::Error& e) {
        QMessageBox::critical(this, QStringLiteral("datagrep"),
                              QStringLiteral("Could not open the engine:\n%1")
                                  .arg(QString::fromUtf8(e.what())));
    }

    // --- sidebar: connection list + management over a lazy schema tree ------
    connections_ = new QListWidget(this);
    connections_->setAlternatingRowColors(true);
    // Wider than square: [marker bar · engine mark] — see dg::engineIcon.
    connections_->setIconSize(QSize(23, 16));
    connect(connections_, &QListWidget::itemSelectionChanged, this,
            &MainWindow::onConnectionSelected);
    // Double-clicking a connection edits it, matching the macOS sidebar.
    connect(connections_, &QListWidget::itemDoubleClicked, this,
            &MainWindow::onEditConnection);

    addButton_ = new QPushButton(QStringLiteral("Add"), this);
    editButton_ = new QPushButton(QStringLiteral("Edit"), this);
    removeButton_ = new QPushButton(QStringLiteral("Remove"), this);
    editButton_->setEnabled(false);
    removeButton_->setEnabled(false);
    connect(addButton_, &QPushButton::clicked, this, &MainWindow::onAddConnection);
    connect(editButton_, &QPushButton::clicked, this, &MainWindow::onEditConnection);
    connect(removeButton_, &QPushButton::clicked, this,
            &MainWindow::onRemoveConnection);

    auto* connButtons = new QWidget(this);
    auto* connButtonsLayout = new QHBoxLayout(connButtons);
    connButtonsLayout->setContentsMargins(4, 4, 4, 0);
    connButtonsLayout->setSpacing(4);
    connButtonsLayout->addWidget(addButton_);
    connButtonsLayout->addWidget(editButton_);
    connButtonsLayout->addWidget(removeButton_);
    connButtonsLayout->addStretch(1);

    auto* connPane = new QWidget(this);
    auto* connLayout = new QVBoxLayout(connPane);
    connLayout->setContentsMargins(0, 0, 0, 0);
    connLayout->setSpacing(0);
    connLayout->addWidget(connButtons);
    connLayout->addWidget(connections_, 1);

    schema_ = new SchemaTree(this);
    schema_->setCore(core_.get());
    connect(schema_, &SchemaTree::objectActivated, this,
            &MainWindow::onSchemaObjectActivated);

    // --- the inspector: schema + cell detail, as a right-hand dock ----------
    // A QDockWidget rather than a fixed pane: closable, floatable, movable to
    // the left — the Linux-native shape for an inspector. Its toggle action
    // lives on the editor toolbar so a closed inspector stays reachable.
    inspector_ = new DetailPanel(this);
    inspectorDock_ = new QDockWidget(QStringLiteral("Inspector"), this);
    inspectorDock_->setObjectName(QStringLiteral("inspectorDock"));
    inspectorDock_->setWidget(inspector_);
    inspectorDock_->setAllowedAreas(Qt::LeftDockWidgetArea |
                                    Qt::RightDockWidgetArea);
    addDockWidget(Qt::RightDockWidgetArea, inspectorDock_);
    // The schema pane follows the sidebar selection; the tree made the describe
    // call, the panel only draws it, so selecting never describes twice.
    connect(schema_, &SchemaTree::objectDescribed, inspector_,
            &DetailPanel::showSchema);
    connect(inspector_, &DetailPanel::cellCopied, this, [this]() {
        status_->showMessage(QStringLiteral("cell JSON copied"));
    });

    // --- query history: a bottom dock over the JSONL store ------------------
    // Hidden until asked for; its toggle lives on the editor toolbar with the
    // shortcut, so closed history stays one keystroke away.
    history_ = new QueryHistoryStore(QueryHistoryStore::defaultDirectory(), this);
    historyPanel_ = new HistoryPanel(history_, this);
    historyDock_ = new QDockWidget(QStringLiteral("Query History"), this);
    historyDock_->setObjectName(QStringLiteral("historyDock"));
    historyDock_->setWidget(historyPanel_);
    historyDock_->setAllowedAreas(Qt::BottomDockWidgetArea |
                                  Qt::TopDockWidgetArea);
    addDockWidget(Qt::BottomDockWidgetArea, historyDock_);
    historyDock_->hide();
    connect(historyPanel_, &HistoryPanel::statusMessage, this,
            [this](const QString& text) { status_->showMessage(text); });
    connect(historyPanel_, &HistoryPanel::openInEditor, this,
            &MainWindow::onOpenHistoryInEditor);
    connect(historyPanel_, &HistoryPanel::rerunRequested, this,
            &MainWindow::onRerunFromHistory);
    history_->load();

    auto* sidebar = new QSplitter(Qt::Vertical, this);
    sidebar->addWidget(connPane);
    sidebar->addWidget(schema_);
    sidebar->setStretchFactor(0, 0);
    sidebar->setStretchFactor(1, 1);

    // --- editors over grid: one unified tab bar across all connections ------
    editors_ = new EditorTabs(this);
    connect(editors_, &EditorTabs::runRequested, this, &MainWindow::runStatement);
    // Binding a tab to a profile moves the window there, so the schema tree
    // and safety surfaces describe the connection the next run will hit.
    connect(editors_, &EditorTabs::connectionBound, this,
            [this](const QString& name) { selectConnection(name); });
    connect(editors_, &EditorTabs::newConnectionRequested, this,
            &MainWindow::onAddConnection);
    connect(editors_, &EditorTabs::statusMessage, this,
            [this](const QString& text) { status_->showMessage(text); });

    auto* editorToolbar = new QToolBar(this);
    auto* runAction = editorToolbar->addAction(QStringLiteral("Run  (Ctrl+↵)"));
    connect(runAction, &QAction::triggered, this, &MainWindow::runStatement);
    editorToolbar->addSeparator();
    editorToolbar->addAction(inspectorDock_->toggleViewAction());
    QAction* historyToggle = historyDock_->toggleViewAction();
    historyToggle->setText(QStringLiteral("History"));
    historyToggle->setShortcut(QKeySequence(QStringLiteral("Ctrl+H")));
    editorToolbar->addAction(historyToggle);

    auto* editorPane = new QWidget(this);
    auto* editorLayout = new QVBoxLayout(editorPane);
    editorLayout->setContentsMargins(0, 0, 0, 0);
    editorLayout->setSpacing(0);
    editorLayout->addWidget(editorToolbar);
    editorLayout->addWidget(editors_);

    model_ = new ResultModel(this);
    connect(model_, &ResultModel::statusChanged, this,
            &MainWindow::onStatusChanged);

    grid_ = new ResultTableView(this);
    grid_->setModel(model_);
    // Clicking a cell shows its full value in the inspector. Only a nested
    // `{n fields}` chip RAISES the Cell tab — that click is an unambiguous
    // request to see inside; a plain value click updates the pane quietly.
    connect(grid_, &QAbstractItemView::clicked, this,
            [this](const QModelIndex& idx) {
                if (!idx.isValid()) {
                    return;
                }
                const auto kind = model_->cellKind(idx.row(), idx.column());
                if (!kind.has_value()) {
                    return;  // skeleton/pending — nothing truthful to show yet
                }
                inspector_->showCell(
                    idx.row(), idx.column(),
                    model_->cellDetailJson(idx.row(), idx.column()),
                    model_->envelopeJson(idx.row()),
                    *kind == dg::CellKind::Nested);
            });

    auto* rightPane = new QSplitter(Qt::Vertical, this);
    rightPane->addWidget(editorPane);
    rightPane->addWidget(grid_);
    rightPane->setStretchFactor(0, 0);
    rightPane->setStretchFactor(1, 1);

    auto* root = new QSplitter(Qt::Horizontal, this);
    root->addWidget(sidebar);
    root->addWidget(rightPane);
    root->setStretchFactor(0, 0);
    root->setStretchFactor(1, 1);
    root->setSizes({260, 900});

    // --- the marked-connection band -----------------------------------------
    // When the selected connection carries a user-chosen colour, this band sits
    // across the whole window above everything else, filled with that colour and
    // carrying the connection's name. The colour means whatever the user meant
    // by it — the band says the name and nothing else. It is deliberately a
    // full-width fill, not a dot or a tint: shrinking it is how these markers
    // stop being noticed. Mirrors the macOS MarkedBanner in intent.
    markedBanner_ = new QLabel(this);
    markedBanner_->setTextFormat(Qt::PlainText);
    markedBanner_->hide();

    auto* central = new QWidget(this);
    auto* centralLayout = new QVBoxLayout(central);
    centralLayout->setContentsMargins(0, 0, 0, 0);
    centralLayout->setSpacing(0);
    centralLayout->addWidget(markedBanner_);
    centralLayout->addWidget(root, 1);
    setCentralWidget(central);

    // --- status bar --------------------------------------------------------
    status_ = new StatusBar(this);
    connect(status_, &StatusBar::cancelRequested, this, &MainWindow::cancelQuery);
    statusBar()->addWidget(status_, 1);

    resize(1200, 760);
    reloadProfiles();
}

MainWindow::~MainWindow() = default;

QString MainWindow::selectedProfile() const {
    const auto* item = connections_->currentItem();
    return item != nullptr ? item->text() : QString();
}

void MainWindow::reloadProfiles() {
    connections_->clear();
    if (!core_) {
        return;
    }
    QString json;
    try {
        json = QString::fromStdString(core_->profilesListJson());
    } catch (const dg::Error& e) {
        status_->showMessage(
            QStringLiteral("profiles: %1").arg(QString::fromUtf8(e.what())), true);
        return;
    }
    const QJsonArray arr = QJsonDocument::fromJson(json.toUtf8()).array();
    safetyByProfile_.clear();
    driverByProfile_.clear();
    QVector<QPair<QString, QString>> connectionOptions;
    for (const QJsonValue& v : arr) {
        const QJsonObject o = v.toObject();
        const QString name = o.value(QStringLiteral("name")).toString();
        const QString driver = o.value(QStringLiteral("driver")).toString();
        const QString env = o.value(QStringLiteral("env")).toString();
        const bool readOnly = o.value(QStringLiteral("read_only")).toBool(false);

        // The safety slice every surface shares — the swatch below, the banner
        // and the run path's confirm-writes prompt all read this one record.
        dg::ConnectionSafety safety;
        safety.name = name;
        safety.color = o.value(QStringLiteral("color")).toString();
        safety.env = env;
        safety.readOnly = readOnly;
        safety.confirmWrites =
            o.value(QStringLiteral("confirm_writes")).toBool(false);
        safetyByProfile_.insert(name, safety);
        driverByProfile_.insert(name, driver);
        connectionOptions.append({name, driver});

        auto* item = new QListWidgetItem(name, connections_);

        // Tooltip carries the facts the row cannot spell out at a glance.
        QStringList tip;
        if (!driver.isEmpty()) {
            tip << dg::engineDisplayName(driver);
        }
        if (!env.isEmpty()) {
            tip << env;
        }
        if (readOnly) {
            tip << QStringLiteral("read-only");
        }

        // Engine mark, with the marker colour as a bar down its leading edge —
        // both visible while scanning the list, not only after selecting. The
        // banner (updateMarkedBanner) is the loud half; this is the
        // recognition cue.
        const auto swatch = dg::connectionColor(safety.color);
        item->setIcon(dg::engineIcon(driver, swatch.value_or(QColor())));
        if (swatch) {
            tip << QStringLiteral("marked %1").arg(safety.color);
        }
        item->setToolTip(tip.join(QStringLiteral(" · ")));

        // A prod row is tinted so it is unmistakable — the guardrail that keeps a
        // production connection from looking like any other. Carried by weight
        // AND colour, never colour alone.
        if (env == QStringLiteral("prod")) {
            item->setForeground(QBrush(QColor(0xC0, 0x39, 0x2B)));
            QFont f = item->font();
            f.setBold(true);
            item->setFont(f);
        }
        // A read-only profile is italicised, so the badge survives in both themes.
        if (readOnly) {
            QFont f = item->font();
            f.setItalic(true);
            item->setFont(f);
        }
    }
    editors_->setConnections(connectionOptions);
    updateMarkedBanner();
}

void MainWindow::onConnectionSelected() {
    const QString profile = selectedProfile();
    const bool have = !profile.isEmpty();
    editButton_->setEnabled(have);
    removeButton_->setEnabled(have);
    updateMarkedBanner();
    editors_->setWindowConnection(profile);
    if (have) {
        schema_->showProfile(profile);
    }
}

void MainWindow::updateMarkedBanner() {
    const QString profile = selectedProfile();
    const dg::ConnectionSafety safety = safetyByProfile_.value(profile);
    const std::optional<QColor> color = dg::connectionColor(safety.color);
    if (profile.isEmpty() || !color) {
        markedBanner_->hide();
        return;
    }
    markedBanner_->setText(profile);
    // Weight AND colour, never colour alone; white text on the user's colour
    // keeps the band legible for every palette entry (all are mid-dark tones).
    markedBanner_->setStyleSheet(
        QStringLiteral("QLabel { background-color: %1; color: white; "
                       "font-weight: 600; padding: 4px 10px; }")
            .arg(color->name()));
    markedBanner_->setAccessibleName(
        QStringLiteral("Marked connection %1").arg(profile));
    markedBanner_->show();
}

void MainWindow::onAddConnection() {
    if (!core_) {
        return;
    }
    ConnectionDialog* dialog = ConnectionDialog::forNewConnection(core_.get(), this);
    if (dialog->exec() == QDialog::Accepted) {
        const QString name = dialog->savedName();
        reloadProfiles();
        // Reselect the newly-added connection so queries scope to it immediately.
        const auto matches = connections_->findItems(name, Qt::MatchExactly);
        if (!matches.isEmpty()) {
            connections_->setCurrentItem(matches.first());
        }
        status_->showMessage(QStringLiteral("added connection ‘%1’").arg(name));
    }
    dialog->deleteLater();
}

void MainWindow::onEditConnection() {
    if (!core_) {
        return;
    }
    const QString profile = selectedProfile();
    if (profile.isEmpty()) {
        return;
    }
    ConnectionDialog* dialog = ConnectionDialog::forEditing(core_.get(), profile, this);
    if (dialog->exec() == QDialog::Accepted) {
        const QString name = dialog->savedName();
        reloadProfiles();
        const auto matches = connections_->findItems(name, Qt::MatchExactly);
        if (!matches.isEmpty()) {
            connections_->setCurrentItem(matches.first());
        }
        status_->showMessage(QStringLiteral("saved connection ‘%1’").arg(name));
    }
    dialog->deleteLater();
}

void MainWindow::onRemoveConnection() {
    if (!core_) {
        return;
    }
    const QString profile = selectedProfile();
    if (profile.isEmpty()) {
        return;
    }
    QMessageBox box(QMessageBox::Warning, QStringLiteral("Remove connection"),
                    QStringLiteral("Remove the connection ‘%1’? This cannot be undone.")
                        .arg(profile),
                    QMessageBox::Cancel, this);
    QPushButton* removeButton =
        box.addButton(QStringLiteral("Remove"), QMessageBox::DestructiveRole);
    box.setDefaultButton(QMessageBox::Cancel);
    box.exec();
    if (box.clickedButton() != removeButton) {
        return;
    }
    try {
        core_->removeProfile(profile.toStdString());
    } catch (const dg::Error& e) {
        status_->showMessage(QString::fromUtf8(e.what()), true);
        return;
    }
    reloadProfiles();
    schema_->showProfile(QString());  // clear the tree for the gone profile
    status_->showMessage(QStringLiteral("removed connection ‘%1’").arg(profile));
}

void MainWindow::runStatement() {
    SqlEditor* editor = editors_->currentEditor();
    if (editor == nullptr) {
        status_->showMessage(
            QStringLiteral("No editor open — Ctrl+T opens one."), true);
        return;
    }
    const QString sql = editor->statementUnderCursor();
    if (sql.isEmpty()) {
        return;
    }
    // Precedence: `-- @connection` in the text, then the tab's binding, then
    // the window's selection.
    QString profile = parseConnectionDirective(sql);
    if (profile.isEmpty()) {
        profile = editors_->activeConnection();
    }
    if (profile.isEmpty()) {
        profile = selectedProfile();
    }
    if (profile.isEmpty()) {
        status_->showMessage(QStringLiteral("Select a connection first."), true);
        return;
    }
    if (!safetyByProfile_.contains(profile)) {
        status_->showMessage(
            QStringLiteral("connection ‘%1’ does not exist — not run").arg(profile),
            true);
        return;
    }
    executeStatement(profile, sql);
}

void MainWindow::executeStatement(const QString& profile, const QString& sql) {
    if (!core_) {
        return;
    }
    // The confirm-writes promise. The engine has no notion of this profile
    // setting, so the prompt lives here: classify the statement, and ask before
    // sending — never after. The classifier is a fat-finger guardrail (first
    // verb only); real refusal on a read-only profile stays with the engine.
    const dg::ConnectionSafety safety = safetyByProfile_.value(profile);
    if (safety.confirmWrites && dg::isWriteStatement(sql)) {
        const QString verb = dg::statementVerb(sql);
        QMessageBox box(QMessageBox::Warning, QStringLiteral("Confirm write"),
                        QStringLiteral("Run a %1 against ‘%2’?").arg(verb, profile),
                        QMessageBox::Cancel, this);
        box.setInformativeText(QStringLiteral(
            "This connection is set to ask before every write. The statement "
            "has not been sent yet."));
        QPushButton* runButton = box.addButton(QStringLiteral("Run %1").arg(verb),
                                               QMessageBox::DestructiveRole);
        box.setDefaultButton(QMessageBox::Cancel);
        box.exec();
        if (box.clickedButton() != runButton) {
            status_->showMessage(
                QStringLiteral("not sent — ‘%1’ asks before every write")
                    .arg(profile));
            return;
        }
    }
    // Recorded before the engine is asked, so a run that never gets a query
    // handle still has a pending entry to fail into.
    history_->executionStarted(sql, profile, driverByProfile_.value(profile));
    try {
        auto query = std::make_unique<dg::Query>(
            core_->run(profile.toStdString(), sql.toStdString()));
        // Reset the status bar's per-result honesty state and tell it whether the
        // statement carried an @limit BEFORE handing the query to the model — the
        // model reads the first status snapshot synchronously inside setQuery, so
        // the hint must already be in place for that first tick.
        status_->beginQuery();
        status_->setLimitHint(parseLimitDirective(sql));
        status_->showMessage(QString());
        model_->setQuery(std::move(query));
        // The cell pane could still be naming a row/column of the previous
        // result; a new query makes that reference meaningless.
        inspector_->clearCell();
    } catch (const dg::Error& e) {
        history_->executionFailedToStart(QString::fromUtf8(e.what()));
        status_->showMessage(QString::fromUtf8(e.what()), true);
    }
}

bool MainWindow::selectConnection(const QString& name) {
    const auto matches = connections_->findItems(name, Qt::MatchExactly);
    if (matches.isEmpty()) {
        return false;
    }
    connections_->setCurrentItem(matches.first());
    return true;
}

void MainWindow::onOpenHistoryInEditor(const QString& sql,
                                       const QString& connection) {
    // A NEW tab, bound to the connection the entry ran against — never
    // overwriting the SQL someone was half way through writing.
    editors_->openInNewTab(sql, connection);
}

void MainWindow::onRerunFromHistory(const QString& sql, const QString& connection) {
    QString profile = connection;
    if (!profile.isEmpty()) {
        // The entry names the connection it ran against; honour it or say why not.
        if (!selectConnection(profile)) {
            status_->showMessage(
                QStringLiteral("connection ‘%1’ no longer exists — not run")
                    .arg(profile),
                true);
            return;
        }
    } else {
        profile = selectedProfile();
        if (profile.isEmpty()) {
            status_->showMessage(QStringLiteral("Select a connection first."), true);
            return;
        }
    }
    executeStatement(profile, sql);
}

void MainWindow::cancelQuery() {
    // The outcome string describes what the SERVER actually did — for engines
    // that cannot truly cancel it says so. model_->cancel() owns the
    // datagrep_query_cancel call and frees its outcome JSON; we only display it.
    const QString outcome = model_->cancel();
    if (!outcome.isEmpty()) {
        // The ABI returns a JSON object; surface its "message" if present, else
        // the raw text.
        const QJsonDocument doc = QJsonDocument::fromJson(outcome.toUtf8());
        const QString message =
            doc.isObject()
                ? doc.object().value(QStringLiteral("message")).toString(outcome)
                : outcome;
        status_->showMessage(message);
    } else {
        status_->showMessage(QStringLiteral("Cancellation requested."));
    }
}

void MainWindow::onStatusChanged(const dg::QueryStatus& status) {
    status_->updateStatus(status);
    // Safe on every tick: this records once, when the query goes terminal.
    history_->executionProgressed(status);
}

void MainWindow::onSchemaObjectActivated(const QString& /*profile*/,
                                         const QString& pathJson) {
    // Insert the object's leaf name at the cursor as a convenience. Building a
    // full, correctly-quoted SELECT is engine-specific and belongs to the
    // engine, not the UI, so we deliberately do not synthesise SQL here.
    const QJsonArray path = QJsonDocument::fromJson(pathJson.toUtf8()).array();
    if (path.isEmpty()) {
        return;
    }
    const QString leaf = path.last().toString();
    SqlEditor* editor = editors_->ensureEditor();
    if (editor == nullptr) {
        return;
    }
    QTextCursor cursor = editor->textCursor();
    cursor.insertText(leaf);
    editor->setFocus();
}
