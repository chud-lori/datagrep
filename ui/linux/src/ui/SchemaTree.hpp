// SchemaTree.hpp — the lazy schema sidebar.

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
    // A leaf/table was double-clicked; MainWindow may turn this into a SELECT.
    void objectActivated(const QString& profile, const QString& pathJson);

    void objectDescribed(const QString& profile, const QString& pathJson,
                         const QString& describeJson, const QString& error);

private slots:
    void onItemExpanded(QTreeWidgetItem* item);
    void onItemActivated(QTreeWidgetItem* item, int column);
    void onCurrentItemChanged(QTreeWidgetItem* current, QTreeWidgetItem* previous);

private:
    void fetchInto(QTreeWidgetItem* parent, const QStringList& fetchPath);

    // A scan_only node refuses to enumerate without a prefix; prompt for one.
    void promptScan(QTreeWidgetItem* node);

    static QString encodePath(const QStringList& segments);

    dg::Core* core_ = nullptr;
    QString profile_;
};

#endif  // DATAGREP_SCHEMA_TREE_HPP
