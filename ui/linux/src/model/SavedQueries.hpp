// SavedQueries.hpp — the on-disk store behind the editor tabs.
//
// Linux counterpart of DatagrepKit.SavedQueries. The engine's `editor_tab`
// table has never been reachable over the C ABI, so — exactly as on macOS —
// each tab is a plain `.sql` file you can open anywhere and commit to git,
// plus a small JSON sidecar for the things SQL cannot carry (which connection
// the tab belongs to, where the caret was), plus `session.json` for tab order
// and the active tab. Same directory layout, same key names, same basename
// rules as the macOS store, so the format stays one format.
//
// One file pair per tab, never one big blob: a half-written blob loses every
// tab, a half-written sidecar loses one tab's caret position.

#ifndef DATAGREP_SAVED_QUERIES_HPP
#define DATAGREP_SAVED_QUERIES_HPP

#include <QString>
#include <QStringList>
#include <QVector>

namespace dg {

// One editor tab as persisted. Scratch tabs (no name) are persisted too —
// losing unsaved SQL because the app crashed is not acceptable.
struct SavedQueryRecord {
    QString id;
    QString name;        // empty = untitled scratch tab
    QString connection;  // empty = the tab follows the window's selection
    int cursorLocation = 0;
    int cursorLength = 0;
    bool isDirty = false;

    bool isScratch() const { return name.isEmpty(); }
    // Basename shared by the `.sql` and the `.json`: a slug for named tabs
    // (readable from a shell), `scratch-<id>` for unnamed ones.
    QString basename() const;
};

// Tab order and the single frontmost tab. The bar shows every open editor at
// once, whatever connection it targets, so there is ONE active tab — not one
// per connection. `activeConnection` only seeds which connection a NEW tab is
// created for.
struct EditorSession {
    QStringList order;
    QString activeID;
    QString activeConnection;
};

}  // namespace dg

// Pure file I/O — no engine, no ABI, no QObject.
class SavedQueryStore {
public:
    explicit SavedQueryStore(const QString& directory = defaultDirectory());

    // <app data>/tabs/, beside the profiles store and the history directory.
    static QString defaultDirectory();
    const QString& directory() const { return directory_; }

    void save(const dg::SavedQueryRecord& record, const QString& text);
    void remove(const dg::SavedQueryRecord& record);
    void saveSession(const dg::EditorSession& session);
    dg::EditorSession loadSession() const;

    struct LoadedTab {
        dg::SavedQueryRecord record;
        QString text;
    };
    struct Loaded {
        QVector<LoadedTab> tabs;
        dg::EditorSession session;
    };
    // Everything on disk in session order. A stale session file costs tab
    // ORDER, never tab CONTENT: forgotten scratch tabs are appended (unsaved
    // work has nowhere else to live), forgotten named tabs stay closed (the
    // saved list is their home).
    Loaded load() const;

    // Every tab on disk, named and scratch alike.
    QVector<dg::SavedQueryRecord> allRecords() const;
    QString text(const dg::SavedQueryRecord& record) const;

    static QString slug(const QString& name);

private:
    QString sqlPath(const dg::SavedQueryRecord& record) const;
    QString sidecarPath(const dg::SavedQueryRecord& record) const;

    QString directory_;
};

#endif  // DATAGREP_SAVED_QUERIES_HPP
