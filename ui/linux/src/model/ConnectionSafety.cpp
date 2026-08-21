#include "ConnectionSafety.hpp"

#include <QStringList>

namespace dg {

std::optional<QColor> connectionColor(const QString& name) {
    // Fixed sRGB values rather than palette roles: the marker must read as the
    // SAME colour on every theme, in the list swatch and in the banner, because
    // it is the user's own code for "careful here". Values are the familiar
    // system hues the macOS palette resolves to.
    const QString n = name.toLower();
    if (n == QStringLiteral("red")) return QColor(0xE0, 0x38, 0x2F);
    if (n == QStringLiteral("orange")) return QColor(0xF2, 0x82, 0x1B);
    if (n == QStringLiteral("yellow")) return QColor(0xDD, 0xA0, 0x00);
    if (n == QStringLiteral("green")) return QColor(0x2E, 0x9E, 0x44);
    if (n == QStringLiteral("blue")) return QColor(0x1E, 0x6F, 0xD9);
    if (n == QStringLiteral("purple")) return QColor(0x8E, 0x44, 0xAD);
    if (n == QStringLiteral("graphite") || n == QStringLiteral("gray") ||
        n == QStringLiteral("grey")) {
        return QColor(0x6E, 0x6E, 0x73);
    }
    return std::nullopt;
}

namespace {

// The statement with leading whitespace and leading `--` comment lines removed
// — directives (`-- @limit …`) and headers sit there and must not be mistaken
// for the verb. Mirrors the skip loop in the macOS classifier exactly.
QString skipLeadingComments(const QString& sql) {
    QString s = sql;
    for (;;) {
        s = s.trimmed();
        if (!s.startsWith(QStringLiteral("--"))) {
            return s;
        }
        const int nl = s.indexOf(QLatin1Char('\n'));
        if (nl < 0) {
            return QString();
        }
        s = s.mid(nl + 1);
    }
}

// The first run of letters at the start of the (comment-stripped) statement.
QString headWord(const QString& sql) {
    const QString s = skipLeadingComments(sql);
    int i = 0;
    while (i < s.size() && s.at(i).isLetter()) {
        ++i;
    }
    return s.left(i);
}

}  // namespace

bool isWriteStatement(const QString& sql) {
    // The same verb list as the macOS classifier — the two apps must agree on
    // what counts as a write, or the same profile behaves differently per OS.
    static const QStringList kWriteVerbs = {
        QStringLiteral("insert"),  QStringLiteral("update"),
        QStringLiteral("delete"),  QStringLiteral("drop"),
        QStringLiteral("truncate"), QStringLiteral("alter"),
        QStringLiteral("create"),  QStringLiteral("grant"),
        QStringLiteral("revoke"),  QStringLiteral("replace"),
        QStringLiteral("merge"),   QStringLiteral("vacuum"),
        QStringLiteral("call"),    QStringLiteral("copy"),
    };
    return kWriteVerbs.contains(headWord(sql).toLower());
}

QString statementVerb(const QString& sql) {
    const QString head = headWord(sql).toUpper();
    return head.isEmpty() ? QStringLiteral("statement") : head;
}

}  // namespace dg
