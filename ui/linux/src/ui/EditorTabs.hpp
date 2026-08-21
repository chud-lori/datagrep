// EditorTabs.hpp — every open SQL editor, in one tab bar.

#ifndef DATAGREP_EDITOR_TABS_HPP
#define DATAGREP_EDITOR_TABS_HPP

#include "model/SavedQueries.hpp"

#include <QPair>
#include <QString>
#include <QTimer>
#include <QVector>
#include <QWidget>

class SqlEditor;
class QComboBox;
class QLabel;
class QPushButton;
class QStackedLayout;
class QTabWidget;
class QToolButton;

class EditorTabs : public QWidget {
    Q_OBJECT

public:
    explicit EditorTabs(QWidget* parent = nullptr);
    ~EditorTabs() override;

    // The active tab's editor, or nullptr when no editor is open.
    SqlEditor* currentEditor() const;
    // The active tab's editor, creating a tab if none is open.
    SqlEditor* ensureEditor();
    // The active tab's bound connection; empty = follow the window.
    QString activeConnection() const;

    void setWindowConnection(const QString& profile);
    void setConnections(const QVector<QPair<QString, QString>>& connections);

    // A NEW tab holding `text`, bound to `connection` (empty = window).
    void openInNewTab(const QString& text, const QString& connection);

public slots:
    void newTab();

signals:
    void runRequested();
    // The user bound the active tab to a profile; the window may follow.
    void connectionBound(const QString& name);
    void newConnectionRequested();
    void statusMessage(const QString& text);

private slots:
    void onCurrentChanged(int index);
    void onCloseRequested(int index);
    void saveActiveTab();

private:
    struct Tab {
        dg::SavedQueryRecord record;
        SqlEditor* editor = nullptr;
        int untitledNumber = 0;
    };

    QString displayTitle(const Tab& tab) const;
    QString driverFor(const QString& connection) const;
    void updateTabChrome(int index);
    int indexOfId(const QString& id) const;
    Tab* activeTab();
    const Tab* activeTab() const;
    void flushTab(Tab& tab);
    SqlEditor* makeEditor(const QString& text, int cursorLocation, int cursorLength);
    void appendTab(const dg::SavedQueryRecord& record, const QString& text,
                   bool activate);
    void performClose(int index, bool keepFiles);
    int nextUntitledNumber(const QString& connection) const;
    void openSaved(const dg::SavedQueryRecord& record);
    void restoreSession();
    void persistSession();
    void persistTab(const Tab& tab);
    void persistEverything();
    void scheduleAutosave();
    void updateWelcomeState();
    void syncBindCombo();
    void rebuildPlusMenu();

    SavedQueryStore store_;
    QVector<Tab> tabs_;  // index-aligned with tabWidget_ pages
    QString windowConnection_;
    QVector<QPair<QString, QString>> connections_;  // name, driver
    bool connectionsAuthoritative_ = false;

    QStackedLayout* stack_;
    QWidget* welcome_;
    QLabel* welcomeBody_;
    QTabWidget* tabWidget_;
    QComboBox* bindCombo_;
    QToolButton* plusButton_;

    QTimer autosaveTimer_;
    bool loading_ = false;  // suppress dirty marks while a document is placed
};

#endif  // DATAGREP_EDITOR_TABS_HPP
