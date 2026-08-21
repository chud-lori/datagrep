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

    // The selected object's describe() payload, for the inspector's schema pane.
    // Fires on EVERY selection change: with the raw describe JSON (fresh or
    // cached — the panel never causes a second describe), with an error string
    // when the describe failed, or with both empty when nothing describable is
    // selected. The panel draws exactly what it is handed, so the tooltip and
    // the pane can never tell two different stories about one object.
    void objectDescribed(const QString& profile, const QString& pathJson,
                         const QString& describeJson, const QString& error);

private slots:
    void onItemExpanded(QTreeWidgetItem* item);
    void onItemActivated(QTreeWidgetItem* item, int column);
    // Lazily describes the newly-selected object (columns/indexes/stats) into its
    // tooltip via datagrep_catalog_describe_json — one object, only when picked,
    // never on expansion.
    void onCurrentItemChanged(QTreeWidgetItem* current, QTreeWidgetItem* previous);

private:
    // Fetches one level of children UNDER `parent` by enumerating `fetchPath`,
    // and populates them. Child paths are built on `fetchPath`, so a scan_only
    // node whose prefix was supplied enumerates fetchPath = node-path + [prefix].
    // Adds a lazy placeholder under any child that itself has children, so the
    // expand arrow appears without fetching that level yet.
    void fetchInto(QTreeWidgetItem* parent, const QStringList& fetchPath);

    // A scan_only node refuses to enumerate without a prefix (the one rule that
    // stops the app firing KEYS * at a 40 GB keyspace). Prompts for a prefix and,
    // if given, enumerates node-path + [prefix] under the node.
    void promptScan(QTreeWidgetItem* node);

    static QString encodePath(const QStringList& segments);

    dg::Core* core_ = nullptr;
    QString profile_;
};

#endif  // DATAGREP_SCHEMA_TREE_HPP
