// SavedQueries.hpp — the on-disk store behind the editor tabs.

#ifndef DATAGREP_SAVED_QUERIES_HPP
#define DATAGREP_SAVED_QUERIES_HPP

#include <QString>
#include <QStringList>
#include <QVector>

namespace dg {

struct SavedQueryRecord {
    QString id;
    QString name;        // empty = untitled scratch tab
    QString connection;  // empty = the tab follows the window's selection
    int cursorLocation = 0;
    int cursorLength = 0;
    bool isDirty = false;

    bool isScratch() const { return name.isEmpty(); }
    QString basename() const;
};

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
