#include "EditorTabs.hpp"

#include "ui/SqlEditor.hpp"

#include <QComboBox>
#include <QFont>
#include <QHash>
#include <QHBoxLayout>
#include <QInputDialog>
#include <QLabel>
#include <QMenu>
#include <QMessageBox>
#include <QPushButton>
#include <QSet>
#include <QShortcut>
#include <QStackedLayout>
#include <QTabWidget>
#include <QTextCursor>
#include <QTextDocument>
#include <QToolButton>
#include <QUuid>
#include <QVBoxLayout>

#include <algorithm>

EditorTabs::EditorTabs(QWidget* parent) : QWidget(parent) {
    tabWidget_ = new QTabWidget(this);
    tabWidget_->setDocumentMode(true);
    tabWidget_->setTabsClosable(true);
    tabWidget_->setElideMode(Qt::ElideRight);
    tabWidget_->setUsesScrollButtons(true);
    connect(tabWidget_, &QTabWidget::currentChanged, this,
            &EditorTabs::onCurrentChanged);
    connect(tabWidget_, &QTabWidget::tabCloseRequested, this,
            &EditorTabs::onCloseRequested);

    // Corner: "+" (new tab, reopen saved) and the active tab's binding picker.
    plusButton_ = new QToolButton(this);
    plusButton_->setText(QStringLiteral("+"));
    plusButton_->setPopupMode(QToolButton::InstantPopup);
    plusButton_->setToolTip(
        QStringLiteral("New query tab (Ctrl+T), or reopen a saved query"));
    auto* plusMenu = new QMenu(plusButton_);
    connect(plusMenu, &QMenu::aboutToShow, this, &EditorTabs::rebuildPlusMenu);
    plusButton_->setMenu(plusMenu);

    bindCombo_ = new QComboBox(this);
    bindCombo_->setToolTip(QStringLiteral(
        "The connection this tab runs against. `-- @connection` inside the "
        "statement still wins."));
    bindCombo_->addItem(QStringLiteral("Follow window connection"), QString());
    // activated, not currentIndexChanged: only a user pick binds; programmatic
    // syncs (tab switches) must not.
    connect(bindCombo_, &QComboBox::activated, this, [this](int index) {
        Tab* tab = activeTab();
        if (tab == nullptr) {
            return;
        }
        const QString name = bindCombo_->itemData(index).toString();
        tab->record.connection = name;
        if (tab->record.isScratch()) {
            tab->untitledNumber = nextUntitledNumber(name);
        }
        updateTabChrome(tabWidget_->currentIndex());
        flushTab(*tab);
        persistTab(*tab);
        persistSession();
        if (!name.isEmpty()) {
            emit connectionBound(name);
        }
    });

    auto* corner = new QWidget(this);
    auto* cornerLayout = new QHBoxLayout(corner);
    cornerLayout->setContentsMargins(2, 0, 4, 0);
    cornerLayout->setSpacing(4);
    cornerLayout->addWidget(plusButton_);
    cornerLayout->addWidget(bindCombo_);
    tabWidget_->setCornerWidget(corner, Qt::TopRightCorner);

    // The welcome pane: what fills the editor area when nothing is open. No
    // manufactured "Untitled 1" and no sample SQL in a dialect that is wrong
    // for every connection but one.
    welcome_ = new QWidget(this);
    auto* welcomeLayout = new QVBoxLayout(welcome_);
    welcomeLayout->addStretch(2);
    auto* welcomeTitle = new QLabel(QStringLiteral("No editor open"), welcome_);
    QFont titleFont = welcomeTitle->font();
    titleFont.setBold(true);
    welcomeTitle->setFont(titleFont);
    welcomeTitle->setAlignment(Qt::AlignHCenter);
    welcomeLayout->addWidget(welcomeTitle);
    welcomeBody_ = new QLabel(
        QStringLiteral(
            "Ctrl+T opens a new SQL editor for the selected connection. Every "
            "editor you open stays in the tab bar, whatever connection it "
            "targets.\n\nCtrl+⏎ runs the statement under the cursor · -- @limit "
            "sets a per-statement row limit"),
        welcome_);
    welcomeBody_->setAlignment(Qt::AlignHCenter);
    welcomeBody_->setWordWrap(true);
    welcomeLayout->addWidget(welcomeBody_);
    auto* welcomeButtons = new QHBoxLayout();
    welcomeButtons->addStretch(1);
    auto* newTabButton = new QPushButton(QStringLiteral("New Query Tab"), welcome_);
    connect(newTabButton, &QPushButton::clicked, this, &EditorTabs::newTab);
    welcomeButtons->addWidget(newTabButton);
    auto* newConnButton =
        new QPushButton(QStringLiteral("New Connection…"), welcome_);
    connect(newConnButton, &QPushButton::clicked, this,
            &EditorTabs::newConnectionRequested);
    welcomeButtons->addWidget(newConnButton);
    welcomeButtons->addStretch(1);
    welcomeLayout->addLayout(welcomeButtons);
    welcomeLayout->addStretch(3);

    stack_ = new QStackedLayout(this);
    stack_->addWidget(welcome_);
    stack_->addWidget(tabWidget_);

    autosaveTimer_.setSingleShot(true);
    autosaveTimer_.setInterval(1200);
    connect(&autosaveTimer_, &QTimer::timeout, this, [this]() {
        Tab* tab = activeTab();
        if (tab != nullptr) {
            flushTab(*tab);
            persistTab(*tab);
        }
    });

    auto* newShortcut = new QShortcut(QKeySequence(QStringLiteral("Ctrl+T")), this);
    newShortcut->setContext(Qt::WindowShortcut);
    connect(newShortcut, &QShortcut::activated, this, &EditorTabs::newTab);
    auto* closeShortcut = new QShortcut(QKeySequence(QStringLiteral("Ctrl+W")), this);
    closeShortcut->setContext(Qt::WindowShortcut);
    connect(closeShortcut, &QShortcut::activated, this, [this]() {
        const int index = tabWidget_->currentIndex();
        if (index >= 0) {
            onCloseRequested(index);
        }
    });
    auto* saveShortcut = new QShortcut(QKeySequence(QStringLiteral("Ctrl+S")), this);
    saveShortcut->setContext(Qt::WindowShortcut);
    connect(saveShortcut, &QShortcut::activated, this, &EditorTabs::saveActiveTab);
    for (int n = 1; n <= 9; ++n) {
        auto* pick = new QShortcut(
            QKeySequence(QStringLiteral("Alt+%1").arg(n)), this);
        pick->setContext(Qt::WindowShortcut);
        connect(pick, &QShortcut::activated, this, [this, n]() {
            if (n <= tabWidget_->count()) {
                tabWidget_->setCurrentIndex(n - 1);
            }
        });
    }

    restoreSession();
    updateWelcomeState();
}

EditorTabs::~EditorTabs() {
    // Unsaved SQL must survive quit as reliably as it survives a crash.
    persistEverything();
}

SqlEditor* EditorTabs::currentEditor() const {
    const int index = tabWidget_->currentIndex();
    if (index < 0 || index >= tabs_.size()) {
        return nullptr;
    }
    return tabs_.at(index).editor;
}

SqlEditor* EditorTabs::ensureEditor() {
    if (SqlEditor* editor = currentEditor()) {
        return editor;
    }
    newTab();
    return currentEditor();
}

QString EditorTabs::activeConnection() const {
    const Tab* tab = activeTab();
    return tab != nullptr ? tab->record.connection : QString();
}

void EditorTabs::setWindowConnection(const QString& profile) {
    if (windowConnection_ == profile) {
        return;
    }
    windowConnection_ = profile;
    persistSession();
}

void EditorTabs::setConnections(const QVector<QPair<QString, QString>>& connections) {
    connections_ = connections;
    connectionsAuthoritative_ = !connections.isEmpty();
    syncBindCombo();

    // Prune tabs bound to connections that no longer exist — but only against
    // an authoritative, non-empty list; guessing would throw away valid tabs.
    // The .sql/.json pairs stay on disk: a missing connection is not a reason
    // to destroy SQL someone wrote.
    if (!connectionsAuthoritative_) {
        return;
    }
    QSet<QString> known;
    for (const auto& c : connections_) {
        known.insert(c.first);
    }
    for (int i = tabs_.size() - 1; i >= 0; --i) {
        const QString bound = tabs_.at(i).record.connection;
        if (!bound.isEmpty() && !known.contains(bound)) {
            performClose(i, /*keepFiles=*/true);
        }
    }
    updateWelcomeState();
}

void EditorTabs::openInNewTab(const QString& text, const QString& connection) {
    dg::SavedQueryRecord record;
    record.id = QUuid::createUuid().toString(QUuid::WithoutBraces);
    record.connection = connection.isEmpty() ? windowConnection_ : connection;
    record.isDirty = true;
    appendTab(record, text, /*activate=*/true);
    Tab& tab = tabs_.last();
    tab.untitledNumber = nextUntitledNumber(record.connection);
    updateTabChrome(tabs_.size() - 1);
    persistTab(tab);
    persistSession();
}

void EditorTabs::newTab() {
    dg::SavedQueryRecord record;
    record.id = QUuid::createUuid().toString(QUuid::WithoutBraces);
    record.connection = windowConnection_;
    appendTab(record, QString(), /*activate=*/true);
    Tab& tab = tabs_.last();
    tab.untitledNumber = nextUntitledNumber(record.connection);
    updateTabChrome(tabs_.size() - 1);
    persistTab(tab);
    persistSession();
    if (SqlEditor* editor = currentEditor()) {
        editor->setFocus();
    }
}

void EditorTabs::onCurrentChanged(int index) {
    Q_UNUSED(index);
    syncBindCombo();
    if (!loading_) {
        persistSession();
        if (SqlEditor* editor = currentEditor()) {
            editor->setFocus();
        }
    }
}

void EditorTabs::onCloseRequested(int index) {
    if (index < 0 || index >= tabs_.size()) {
        return;
    }
    Tab& tab = tabs_[index];
    flushTab(tab);
    const QString text = tab.editor->toPlainText();

    // Closing an unsaved scratch tab is the ONLY action that destroys typed
    // SQL — quitting keeps everything — so it is the one place a confirmation
    // belongs, and there is deliberately none on quit.
    if (tab.record.isScratch() && tab.record.isDirty &&
        !text.trimmed().isEmpty()) {
        QMessageBox box(QMessageBox::Warning, QStringLiteral("Discard this query?"),
                        QStringLiteral("%1 has not been saved. Closing the tab "
                                       "deletes it — quitting datagrep would "
                                       "keep it.")
                            .arg(displayTitle(tab)),
                        QMessageBox::Cancel, this);
        QPushButton* discardButton =
            box.addButton(QStringLiteral("Discard"), QMessageBox::DestructiveRole);
        QPushButton* saveButton =
            box.addButton(QStringLiteral("Save…"), QMessageBox::ActionRole);
        box.setDefaultButton(QMessageBox::Cancel);
        box.exec();
        if (box.clickedButton() == saveButton) {
            tabWidget_->setCurrentIndex(index);
            saveActiveTab();
            // Backing out of the name prompt is not consent to discard.
            if (tabs_.at(index).record.isScratch()) {
                return;
            }
            performClose(index, /*keepFiles=*/true);
            return;
        }
        if (box.clickedButton() != discardButton) {
            return;
        }
        performClose(index, /*keepFiles=*/false);
        return;
    }
    // A named query is a file the user asked us to keep: closing its tab drops
    // it from the session, never from the disk.
    performClose(index, /*keepFiles=*/!tab.record.isScratch());
}

void EditorTabs::performClose(int index, bool keepFiles) {
    const Tab tab = tabs_.at(index);
    tabs_.removeAt(index);  // before removeTab: currentChanged reads tabs_
    tabWidget_->removeTab(index);
    tab.editor->deleteLater();
    if (!keepFiles) {
        store_.remove(tab.record);
    }
    persistSession();
    updateWelcomeState();
}

void EditorTabs::saveActiveTab() {
    const int index = tabWidget_->currentIndex();
    if (index < 0 || index >= tabs_.size()) {
        return;
    }
    Tab& tab = tabs_[index];
    flushTab(tab);
    const QString text = tab.editor->toPlainText();

    if (tab.record.isScratch()) {
        // First few words of the statement beat "Untitled" as a suggestion.
        QStringList words;
        const QStringList raw =
            text.simplified().split(QLatin1Char(' '), Qt::SkipEmptyParts);
        for (const QString& w : raw) {
            if (w.startsWith(QStringLiteral("--"))) {
                continue;
            }
            words << w;
            if (words.size() == 4) {
                break;
            }
        }
        QString suggestion = words.join(QLatin1Char(' '));
        while (suggestion.endsWith(QLatin1Char(';')) ||
               suggestion.endsWith(QLatin1Char(',')) ||
               suggestion.endsWith(QLatin1Char(' '))) {
            suggestion.chop(1);
        }
        if (suggestion.isEmpty()) {
            suggestion = QStringLiteral("query");
        }
        bool ok = false;
        const QString name =
            QInputDialog::getText(
                this, QStringLiteral("Save Query"),
                QStringLiteral("Saved as a plain .sql file in %1 — readable in "
                               "any editor, and committable to git.")
                    .arg(store_.directory()),
                QLineEdit::Normal, suggestion.left(48), &ok)
                .trimmed();
        if (!ok || name.isEmpty()) {
            return;
        }
        // Write the new pair before dropping the old basename's, never after.
        const dg::SavedQueryRecord previous = tab.record;
        tab.record.name = name;
        tab.record.isDirty = false;
        store_.save(tab.record, text);
        store_.remove(previous);
    } else {
        tab.record.isDirty = false;
        store_.save(tab.record, text);
    }
    updateTabChrome(index);
    persistSession();
    emit statusMessage(QStringLiteral("saved ‘%1’").arg(tab.record.name));
}

QString EditorTabs::displayTitle(const Tab& tab) const {
    if (!tab.record.name.isEmpty()) {
        return tab.record.name;
    }
    return tab.untitledNumber > 0
               ? QStringLiteral("Untitled %1").arg(tab.untitledNumber)
               : QStringLiteral("Untitled");
}

void EditorTabs::updateTabChrome(int index) {
    if (index < 0 || index >= tabs_.size()) {
        return;
    }
    const Tab& tab = tabs_.at(index);
    QString title = displayTitle(tab);
    // Two "Untitled 1" on different connections must differ in the bar.
    if (!tab.record.connection.isEmpty()) {
        title += QStringLiteral(" · ") + tab.record.connection;
    }
    if (tab.record.isDirty) {
        title += QStringLiteral(" •");
    }
    tabWidget_->setTabText(index, title);
    QStringList tip;
    tip << (tab.record.isScratch()
                ? QStringLiteral("Unsaved scratch tab — Ctrl+S names it")
                : QStringLiteral("%1 · Ctrl+S saves").arg(tab.record.name));
    tip << (tab.record.connection.isEmpty()
                ? QStringLiteral("follows the window connection")
                : QStringLiteral("runs against ‘%1’").arg(tab.record.connection));
    tabWidget_->setTabToolTip(index, tip.join(QStringLiteral("\n")));
}

int EditorTabs::indexOfId(const QString& id) const {
    for (int i = 0; i < tabs_.size(); ++i) {
        if (tabs_.at(i).record.id == id) {
            return i;
        }
    }
    return -1;
}

EditorTabs::Tab* EditorTabs::activeTab() {
    const int index = tabWidget_->currentIndex();
    return (index >= 0 && index < tabs_.size()) ? &tabs_[index] : nullptr;
}

const EditorTabs::Tab* EditorTabs::activeTab() const {
    const int index = tabWidget_->currentIndex();
    return (index >= 0 && index < tabs_.size()) ? &tabs_.at(index) : nullptr;
}

void EditorTabs::flushTab(Tab& tab) {
    const QTextCursor cursor = tab.editor->textCursor();
    tab.record.cursorLocation = cursor.selectionStart();
    tab.record.cursorLength = cursor.selectionEnd() - cursor.selectionStart();
}

SqlEditor* EditorTabs::makeEditor(const QString& text, int cursorLocation,
                                  int cursorLength) {
    auto* editor = new SqlEditor(this);
    loading_ = true;
    editor->setPlainText(text);
    const int length = editor->document()->characterCount() - 1;  // trailing ¶
    const int loc = std::clamp(cursorLocation, 0, std::max(0, length));
    const int len = std::clamp(cursorLength, 0, std::max(0, length - loc));
    QTextCursor cursor(editor->document());
    cursor.setPosition(loc);
    if (len > 0) {
        cursor.setPosition(loc + len, QTextCursor::KeepAnchor);
    }
    editor->setTextCursor(cursor);
    loading_ = false;

    connect(editor, &SqlEditor::runRequested, this, &EditorTabs::runRequested);
    connect(editor->document(), &QTextDocument::contentsChanged, this,
            [this, editor]() {
                if (loading_) {
                    return;
                }
                const int index = tabWidget_->indexOf(editor);
                if (index < 0 || index >= tabs_.size()) {
                    return;
                }
                if (!tabs_.at(index).record.isDirty) {
                    tabs_[index].record.isDirty = true;
                    updateTabChrome(index);
                }
                scheduleAutosave();
            });
    return editor;
}

void EditorTabs::appendTab(const dg::SavedQueryRecord& record, const QString& text,
                           bool activate) {
    Tab tab;
    tab.record = record;
    tab.editor = makeEditor(text, record.cursorLocation, record.cursorLength);
    tabs_.append(tab);
    tabWidget_->addTab(tab.editor, QString());
    updateTabChrome(tabs_.size() - 1);
    updateWelcomeState();
    if (activate) {
        tabWidget_->setCurrentIndex(tabs_.size() - 1);
    }
}

// Lowest unused number within one connection, so every connection starts at
// "Untitled 1" rather than continuing a global count.
int EditorTabs::nextUntitledNumber(const QString& connection) const {
    QSet<int> used;
    for (const Tab& tab : tabs_) {
        if (tab.record.isScratch() && tab.record.connection == connection) {
            used.insert(tab.untitledNumber);
        }
    }
    int n = 1;
    while (used.contains(n)) {
        ++n;
    }
    return n;
}

void EditorTabs::openSaved(const dg::SavedQueryRecord& record) {
    const int existing = indexOfId(record.id);
    if (existing >= 0) {
        tabWidget_->setCurrentIndex(existing);
        return;
    }
    const QString text = store_.text(record);
    appendTab(record, text, /*activate=*/true);
    Tab& tab = tabs_.last();
    if (tab.record.isScratch()) {
        tab.untitledNumber = nextUntitledNumber(tab.record.connection);
        updateTabChrome(tabs_.size() - 1);
    }
    persistSession();
}

void EditorTabs::restoreSession() {
    loading_ = true;
    const SavedQueryStore::Loaded loaded = store_.load();
    for (const SavedQueryStore::LoadedTab& t : loaded.tabs) {
        appendTab(t.record, t.text, /*activate=*/false);
    }
    // Renumber untitled tabs per connection in restore order, so two
    // connections can each have an "Untitled 1".
    QHash<QString, int> counters;
    for (int i = 0; i < tabs_.size(); ++i) {
        if (tabs_.at(i).record.isScratch()) {
            const int n = counters.value(tabs_.at(i).record.connection, 0) + 1;
            counters.insert(tabs_.at(i).record.connection, n);
            tabs_[i].untitledNumber = n;
            updateTabChrome(i);
        }
    }
    windowConnection_ = loaded.session.activeConnection;
    const int active = indexOfId(loaded.session.activeID);
    if (active >= 0) {
        tabWidget_->setCurrentIndex(active);
    }
    loading_ = false;
    syncBindCombo();
}

void EditorTabs::persistSession() {
    dg::EditorSession session;
    for (const Tab& tab : tabs_) {
        session.order.append(tab.record.id);
    }
    const Tab* active = activeTab();
    session.activeID = active != nullptr ? active->record.id : QString();
    session.activeConnection = windowConnection_;
    store_.saveSession(session);
}

void EditorTabs::persistTab(const Tab& tab) {
    store_.save(tab.record, tab.editor->toPlainText());
}

void EditorTabs::persistEverything() {
    autosaveTimer_.stop();
    for (Tab& tab : tabs_) {
        flushTab(tab);
        persistTab(tab);
    }
    persistSession();
}

void EditorTabs::scheduleAutosave() {
    if (!autosaveTimer_.isActive()) {
        autosaveTimer_.start();
    }
}

void EditorTabs::updateWelcomeState() {
    stack_->setCurrentWidget(tabs_.isEmpty() ? welcome_ : tabWidget_);
}

void EditorTabs::syncBindCombo() {
    bindCombo_->clear();
    bindCombo_->addItem(QStringLiteral("Follow window connection"), QString());
    for (const auto& c : connections_) {
        const QString label = c.second.isEmpty()
                                  ? c.first
                                  : QStringLiteral("%1 · %2").arg(c.first, c.second);
        bindCombo_->addItem(label, c.first);
    }
    const Tab* active = activeTab();
    const QString bound = active != nullptr ? active->record.connection : QString();
    int select = 0;
    for (int i = 1; i < bindCombo_->count(); ++i) {
        if (bindCombo_->itemData(i).toString() == bound) {
            select = i;
            break;
        }
    }
    // A restored binding to a profile the list does not know yet still has to
    // be visible, or the binding would silently read as "window".
    if (select == 0 && !bound.isEmpty()) {
        bindCombo_->addItem(bound, bound);
        select = bindCombo_->count() - 1;
    }
    bindCombo_->setCurrentIndex(select);
    bindCombo_->setEnabled(active != nullptr);
}

void EditorTabs::rebuildPlusMenu() {
    QMenu* menu = plusButton_->menu();
    menu->clear();
    menu->addAction(QStringLiteral("New Query Tab"), this, &EditorTabs::newTab);

    // Closed named queries, the way back to a .sql the bar no longer shows.
    // Records bound to a gone connection are hidden under the same rule as the
    // orphan pruning, so pruned tabs do not reappear as stale entries.
    QSet<QString> open;
    for (const Tab& tab : tabs_) {
        open.insert(tab.record.id);
    }
    QSet<QString> known;
    for (const auto& c : connections_) {
        known.insert(c.first);
    }
    QVector<dg::SavedQueryRecord> reopenable;
    for (const dg::SavedQueryRecord& r : store_.allRecords()) {
        if (open.contains(r.id)) {
            continue;
        }
        if (connectionsAuthoritative_ && !r.connection.isEmpty() &&
            !known.contains(r.connection)) {
            continue;
        }
        reopenable.append(r);
    }
    std::sort(reopenable.begin(), reopenable.end(),
              [](const dg::SavedQueryRecord& a, const dg::SavedQueryRecord& b) {
                  return (a.name.isEmpty() ? a.id : a.name) <
                         (b.name.isEmpty() ? b.id : b.name);
              });
    if (!reopenable.isEmpty()) {
        menu->addSeparator();
        for (const dg::SavedQueryRecord& r : reopenable) {
            const QString label =
                r.name.isEmpty() ? QStringLiteral("Untitled") : r.name;
            menu->addAction(label, this, [this, r]() { openSaved(r); });
        }
    }
}
