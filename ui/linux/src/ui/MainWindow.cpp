#include "MainWindow.hpp"

#include "ffi/DatagrepFfi.hpp"
#include "model/ResultModel.hpp"
#include "ui/ConnectionDialog.hpp"
#include "ui/ResultTableView.hpp"
#include "ui/SchemaTree.hpp"
#include "ui/SqlEditor.hpp"
#include "ui/StatusBar.hpp"

#include <QAction>
#include <QBrush>
#include <QColor>
#include <QDir>
#include <QFont>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
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

    auto* sidebar = new QSplitter(Qt::Vertical, this);
    sidebar->addWidget(connPane);
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
    for (const QJsonValue& v : arr) {
        const QJsonObject o = v.toObject();
        const QString name = o.value(QStringLiteral("name")).toString();
        const QString driver = o.value(QStringLiteral("driver")).toString();
        const QString env = o.value(QStringLiteral("env")).toString();
        const bool readOnly = o.value(QStringLiteral("read_only")).toBool(false);

        auto* item = new QListWidgetItem(name, connections_);

        // Tooltip carries the facts the row cannot spell out at a glance.
        QStringList tip;
        if (!driver.isEmpty()) {
            tip << driver;
        }
        if (!env.isEmpty()) {
            tip << env;
        }
        if (readOnly) {
            tip << QStringLiteral("read-only");
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
}

void MainWindow::onConnectionSelected() {
    const QString profile = selectedProfile();
    const bool have = !profile.isEmpty();
    editButton_->setEnabled(have);
    removeButton_->setEnabled(have);
    if (have) {
        schema_->showProfile(profile);
    }
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
    if (!core_) {
        return;
    }
    const QString profile = selectedProfile();
    if (profile.isEmpty()) {
        status_->showMessage(QStringLiteral("Select a connection first."), true);
        return;
    }
    const QString sql = editor_->statementUnderCursor();
    if (sql.isEmpty()) {
        return;
    }
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
    } catch (const dg::Error& e) {
        status_->showMessage(QString::fromUtf8(e.what()), true);
    }
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
