#include "MainWindow.hpp"

#include "ffi/DatagrepFfi.hpp"
#include "model/ResultModel.hpp"
#include "ui/ResultTableView.hpp"
#include "ui/SchemaTree.hpp"
#include "ui/SqlEditor.hpp"

#include <QAction>
#include <QDir>
#include <QGuiApplication>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QListWidget>
#include <QMessageBox>
#include <QPushButton>
#include <QSplitter>
#include <QStandardPaths>
#include <QStatusBar>
#include <QTextCursor>
#include <QToolBar>
#include <QVBoxLayout>
#include <QWidget>

namespace {

// The profiles store lives in the platform's per-user app data directory. The
// engine (datagrep_core_new) opens/creates the SQLite file at this path.
QString profilesDbPath() {
    const QString dir =
        QStandardPaths::writableLocation(QStandardPaths::AppDataLocation);
    QDir().mkpath(dir);
    return dir + QStringLiteral("/profiles.db");
}

QString formatCount(std::uint64_t n) {
    // Grouped with thin separators for the status bar (the grid gutter stays
    // ungrouped; this is prose, not a row number).
    QString s = QString::number(static_cast<qulonglong>(n));
    int pos = s.size() - 3;
    while (pos > 0) {
        s.insert(pos, QLatin1Char(','));
        pos -= 3;
    }
    return s;
}

QString formatElapsed(std::uint64_t ms) {
    if (ms < 1000) {
        return QStringLiteral("%1 ms").arg(ms);
    }
    return QStringLiteral("%1 s").arg(QString::number(ms / 1000.0, 'f', 2));
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

    // --- sidebar: connections over a lazy schema tree ----------------------
    connections_ = new QListWidget(this);
    connections_->setAlternatingRowColors(true);
    connect(connections_, &QListWidget::itemSelectionChanged, this,
            &MainWindow::onConnectionSelected);

    schema_ = new SchemaTree(this);
    schema_->setCore(core_.get());
    connect(schema_, &SchemaTree::objectActivated, this,
            &MainWindow::onSchemaObjectActivated);

    auto* sidebar = new QSplitter(Qt::Vertical, this);
    sidebar->addWidget(connections_);
    sidebar->addWidget(schema_);
    sidebar->setStretchFactor(0, 0);
    sidebar->setStretchFactor(1, 1);

    // --- editor over grid --------------------------------------------------
    editor_ = new SqlEditor(this);
    connect(editor_, &SqlEditor::runRequested, this, &MainWindow::runStatement);

    auto* editorToolbar = new QToolBar(this);
    auto* runAction = editorToolbar->addAction(QStringLiteral("Run  (Ctrl+↵)"));
    connect(runAction, &QAction::triggered, this, &MainWindow::runStatement);

    auto* editorPane = new QWidget(this);
    auto* editorLayout = new QVBoxLayout(editorPane);
    editorLayout->setContentsMargins(0, 0, 0, 0);
    editorLayout->setSpacing(0);
    editorLayout->addWidget(editorToolbar);
    editorLayout->addWidget(editor_);

    model_ = new ResultModel(this);
    connect(model_, &ResultModel::statusChanged, this,
            &MainWindow::onStatusChanged);

    grid_ = new ResultTableView(this);
    grid_->setModel(model_);

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
    setCentralWidget(root);

    // --- status bar: rows / elapsed / state / read-only / cancel -----------
    rowsLabel_ = new QLabel(this);
    elapsedLabel_ = new QLabel(this);
    stateLabel_ = new QLabel(this);
    readOnlyLabel_ = new QLabel(this);
    messageLabel_ = new QLabel(this);
    messageLabel_->setTextInteractionFlags(Qt::TextSelectableByMouse);
    cancelButton_ = new QPushButton(QStringLiteral("Cancel"), this);
    cancelButton_->setEnabled(false);
    connect(cancelButton_, &QPushButton::clicked, this, &MainWindow::cancelQuery);

    statusBar()->addWidget(stateLabel_);
    statusBar()->addWidget(rowsLabel_);
    statusBar()->addWidget(elapsedLabel_);
    statusBar()->addWidget(readOnlyLabel_);
    statusBar()->addWidget(messageLabel_, 1);
    statusBar()->addPermanentWidget(cancelButton_);

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
        messageLabel_->setText(
            QStringLiteral("profiles: %1").arg(QString::fromUtf8(e.what())));
        return;
    }
    const QJsonArray arr = QJsonDocument::fromJson(json.toUtf8()).array();
    for (const QJsonValue& v : arr) {
        const QJsonObject o = v.toObject();
        const QString name = o.value(QStringLiteral("name")).toString();
        const QString driver = o.value(QStringLiteral("driver")).toString();
        auto* item = new QListWidgetItem(name, connections_);
        item->setToolTip(driver);
    }
}

void MainWindow::onConnectionSelected() {
    const QString profile = selectedProfile();
    if (!profile.isEmpty()) {
        schema_->showProfile(profile);
    }
}

void MainWindow::runStatement() {
    if (!core_) {
        return;
    }
    const QString profile = selectedProfile();
    if (profile.isEmpty()) {
        messageLabel_->setText(QStringLiteral("Select a connection first."));
        return;
    }
    const QString sql = editor_->statementUnderCursor();
    if (sql.isEmpty()) {
        return;
    }
    try {
        auto query = std::make_unique<dg::Query>(
            core_->run(profile.toStdString(), sql.toStdString()));
        model_->setQuery(std::move(query));
        cancelButton_->setEnabled(true);
        messageLabel_->clear();
    } catch (const dg::Error& e) {
        messageLabel_->setText(QString::fromUtf8(e.what()));
    }
}

void MainWindow::cancelQuery() {
    // The outcome string describes what the SERVER actually did — for engines
    // that cannot truly cancel it says so. Shown to the user verbatim.
    const QString outcome = model_->cancel();
    if (!outcome.isEmpty()) {
        // The ABI returns a JSON object; surface its "message" if present, else
        // the raw text.
        const QJsonDocument doc = QJsonDocument::fromJson(outcome.toUtf8());
        const QString message =
            doc.isObject()
                ? doc.object().value(QStringLiteral("message")).toString(outcome)
                : outcome;
        messageLabel_->setText(message);
    } else {
        messageLabel_->setText(QStringLiteral("Cancellation requested."));
    }
}

void MainWindow::onStatusChanged(const dg::QueryStatus& status) {
    // State chip.
    QString stateText;
    switch (status.state) {
        case dg::QueryState::Streaming: stateText = QStringLiteral("Streaming…"); break;
        case dg::QueryState::Parked: stateText = QStringLiteral("Parked"); break;
        case dg::QueryState::Capped: stateText = QStringLiteral("Capped"); break;
        case dg::QueryState::Done: stateText = QStringLiteral("Done"); break;
        case dg::QueryState::Cancelled: stateText = QStringLiteral("Cancelled"); break;
        case dg::QueryState::Failed: stateText = QStringLiteral("Failed"); break;
    }
    stateLabel_->setText(stateText);

    // Row count, honest about whether it is final. A capped result must SAY it is
    // capped — the count is the server's cap, not the table's size.
    if (status.affectedRows.has_value()) {
        rowsLabel_->setText(
            QStringLiteral("%1 affected").arg(formatCount(*status.affectedRows)));
    } else if (status.capped()) {
        rowsLabel_->setText(QStringLiteral("first %1 rows (server capped)")
                                .arg(formatCount(status.rowsLoaded)));
    } else if (status.streaming()) {
        rowsLabel_->setText(
            QStringLiteral("%1 rows so far…").arg(formatCount(status.rowsLoaded)));
    } else if (status.totalKnown) {
        rowsLabel_->setText(QStringLiteral("%1 rows").arg(formatCount(status.rowsLoaded)));
    } else {
        rowsLabel_->setText(
            QStringLiteral("%1 rows (partial)").arg(formatCount(status.rowsLoaded)));
    }

    elapsedLabel_->setText(formatElapsed(status.elapsedMs));

    // Read-only badge — name WHICH protection is in force, never imply server
    // enforcement that is not there (matching the ABI's honesty contract).
    if (status.readOnlyEnforcement.isEmpty()) {
        readOnlyLabel_->clear();
    } else if (status.readOnlyEnforcement == QStringLiteral("server")) {
        readOnlyLabel_->setText(status.readOnlyServerConfirmed
                                    ? QStringLiteral("read-only (server)")
                                    : QStringLiteral("read-only (server, unconfirmed)"));
    } else if (status.readOnlyEnforcement == QStringLiteral("client")) {
        readOnlyLabel_->setText(QStringLiteral("read-only (client only)"));
    } else {
        readOnlyLabel_->setText(QStringLiteral("read-only (none)"));
    }

    if (!status.error.isEmpty()) {
        messageLabel_->setText(status.error);
    }

    // The cancel button is live only while there is something to cancel.
    cancelButton_->setEnabled(status.streaming());
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
    QTextCursor cursor = editor_->textCursor();
    cursor.insertText(leaf);
    editor_->setFocus();
}
