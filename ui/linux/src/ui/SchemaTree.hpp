// SchemaTree.hpp — the lazy schema sidebar.
//
// Mirrors the macOS SchemaPane: ONE level is fetched per expansion, never a
// crawl, via datagrep_catalog_children_json (JSON path array in, one level of
// children out). A node is only ever fetched when the user actually expands it,
// so a 40 GB Redis is never enumerated by opening the tree. The `enumeration`
// field ("cheap"|"scan_only"|"paged"|"on_demand") gates auto-expansion: only
// "cheap" nodes may expand on their own — the single rule that stops the app
// firing KEYS * at a huge keyspace.
//
// This is UI glue only: it holds no schema logic, it just walks the catalog ABI.

#ifndef DATAGREP_SCHEMA_TREE_HPP
#define DATAGREP_SCHEMA_TREE_HPP

#include <QTreeWidget>

namespace dg {
class Core;
}
class QTreeWidgetItem;

class SchemaTree : public QTreeWidget {
    Q_OBJECT

public:
    explicit SchemaTree(QWidget* parent = nullptr);

    // `core` is borrowed and must outlive this widget.
    void setCore(dg::Core* core) { core_ = core; }

    // Load the roots (path []) for a profile. Clears any previous tree.
    void showProfile(const QString& profile);

signals:
    // A leaf/table was activated (double-clicked) — the MainWindow may turn this
    // into a SELECT. Carries profile + JSON path array.
    void objectActivated(const QString& profile, const QString& pathJson);

private slots:
    void onItemExpanded(QTreeWidgetItem* item);
    void onItemActivated(QTreeWidgetItem* item, int column);

private:
    // Fetches one level of children for `item` (or the roots when item==nullptr)
    // and populates them. Adds a lazy placeholder under any child that itself has
    // children, so the expand arrow appears without fetching that level yet.
    void populateChildren(QTreeWidgetItem* item);

    static QString encodePath(const QStringList& segments);

    dg::Core* core_ = nullptr;
    QString profile_;
};

#endif  // DATAGREP_SCHEMA_TREE_HPP
