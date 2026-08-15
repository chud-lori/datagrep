#include "SchemaTree.hpp"

#include "ffi/DatagrepFfi.hpp"

#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QStringList>
#include <QTreeWidgetItem>

namespace {
// Item data roles carried on each tree node.
constexpr int kPathRole = Qt::UserRole + 1;         // QStringList: full path
constexpr int kHasChildrenRole = Qt::UserRole + 2;  // bool
constexpr int kEnumerationRole = Qt::UserRole + 3;  // QString
constexpr int kLoadedRole = Qt::UserRole + 4;       // bool: children fetched
}  // namespace

SchemaTree::SchemaTree(QWidget* parent) : QTreeWidget(parent) {
    setHeaderHidden(true);
    setColumnCount(1);
    setUniformRowHeights(true);
    setExpandsOnDoubleClick(false);
    connect(this, &QTreeWidget::itemExpanded, this, &SchemaTree::onItemExpanded);
    connect(this, &QTreeWidget::itemActivated, this, &SchemaTree::onItemActivated);
}

void SchemaTree::showProfile(const QString& profile) {
    profile_ = profile;
    clear();
    if (core_ == nullptr || profile_.isEmpty()) {
        return;
    }
    populateChildren(nullptr);  // roots: path []
}

QString SchemaTree::encodePath(const QStringList& segments) {
    QJsonArray arr;
    for (const QString& s : segments) {
        arr.append(s);
    }
    return QString::fromUtf8(QJsonDocument(arr).toJson(QJsonDocument::Compact));
}

void SchemaTree::populateChildren(QTreeWidgetItem* item) {
    if (core_ == nullptr) {
        return;
    }
    const QStringList path =
        item != nullptr ? item->data(0, kPathRole).toStringList() : QStringList();

    QString json;
    try {
        json = QString::fromStdString(
            core_->catalogChildrenJson(profile_.toStdString(),
                                       encodePath(path).toStdString()));
    } catch (const dg::Error& e) {
        // Surface the failure inline rather than silently showing an empty node.
        auto* err = item != nullptr ? new QTreeWidgetItem(item)
                                    : new QTreeWidgetItem(this);
        err->setText(0, QStringLiteral("⚠ %1").arg(QString::fromUtf8(e.what())));
        err->setFlags(Qt::ItemIsEnabled);
        return;
    }

    const QJsonArray children = QJsonDocument::fromJson(json.toUtf8()).array();
    for (const QJsonValue& v : children) {
        const QJsonObject o = v.toObject();
        const QString name = o.value(QStringLiteral("name")).toString();
        const QString kind = o.value(QStringLiteral("kind")).toString();
        const bool hasChildren =
            o.value(QStringLiteral("has_children")).toBool(false);
        const QString enumeration =
            o.value(QStringLiteral("enumeration")).toString();

        QStringList childPath = path;
        childPath << name;

        auto* node =
            item != nullptr ? new QTreeWidgetItem(item) : new QTreeWidgetItem(this);
        node->setText(0, name);
        node->setToolTip(0, kind);
        node->setData(0, kPathRole, childPath);
        node->setData(0, kHasChildrenRole, hasChildren);
        node->setData(0, kEnumerationRole, enumeration);
        node->setData(0, kLoadedRole, false);

        if (hasChildren) {
            // A lazy placeholder gives the expand arrow without fetching the
            // next level yet. It is replaced on first expansion.
            auto* placeholder = new QTreeWidgetItem(node);
            placeholder->setText(0, QStringLiteral("…"));
            placeholder->setFlags(Qt::NoItemFlags);
            // Only "cheap" nodes may auto-expand; everything else waits for the
            // user, so a scan-only / paged / on-demand keyspace is never crawled.
            if (enumeration == QStringLiteral("cheap")) {
                node->setExpanded(true);  // triggers onItemExpanded -> real fetch
            }
        }
    }
    if (item != nullptr) {
        item->setData(0, kLoadedRole, true);
    }
}

void SchemaTree::onItemExpanded(QTreeWidgetItem* item) {
    if (item == nullptr || item->data(0, kLoadedRole).toBool()) {
        return;  // already fetched
    }
    // Drop the lazy placeholder(s), then fetch the real level.
    const auto placeholders = item->takeChildren();
    for (QTreeWidgetItem* p : placeholders) {
        delete p;
    }
    populateChildren(item);
}

void SchemaTree::onItemActivated(QTreeWidgetItem* item, int /*column*/) {
    if (item == nullptr) {
        return;
    }
    const QStringList path = item->data(0, kPathRole).toStringList();
    if (path.isEmpty()) {
        return;
    }
    emit objectActivated(profile_, encodePath(path));
}
