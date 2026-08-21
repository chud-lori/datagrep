#include "SqlHighlighter.hpp"

#include <QColor>

SqlHighlighter::SqlHighlighter(QTextDocument* document)
    : QSyntaxHighlighter(document) {
    keywordFormat_.setForeground(QColor(0x56, 0x9c, 0xd6));
    keywordFormat_.setFontWeight(QFont::DemiBold);
    stringFormat_.setForeground(QColor(0xce, 0x91, 0x78));
    numberFormat_.setForeground(QColor(0xb5, 0xce, 0xa8));
    commentFormat_.setForeground(QColor(0x6a, 0x99, 0x55));
    commentFormat_.setFontItalic(true);

    static const char* const kKeywords[] = {
        "SELECT",  "FROM",     "WHERE",   "INSERT", "UPDATE",  "DELETE",
        "CREATE",  "DROP",     "ALTER",   "TABLE",  "INDEX",   "VIEW",
        "INTO",    "VALUES",   "SET",     "JOIN",   "INNER",   "LEFT",
        "RIGHT",   "OUTER",    "FULL",    "ON",     "AS",      "AND",
        "OR",      "NOT",      "NULL",    "IS",     "IN",      "LIKE",
        "BETWEEN", "GROUP",    "BY",      "ORDER",  "HAVING",  "LIMIT",
        "OFFSET",  "DISTINCT", "COUNT",   "SUM",    "AVG",     "MIN",
        "MAX",     "UNION",    "ALL",     "CASE",   "WHEN",    "THEN",
        "ELSE",    "END",      "ASC",     "DESC",   "PRIMARY", "KEY",
        "FOREIGN", "REFERENCES", "DEFAULT", "WITH",  "RETURNING",
    };
    for (const char* kw : kKeywords) {
        Rule rule;
        rule.pattern = QRegularExpression(
            QStringLiteral("\\b%1\\b").arg(QLatin1String(kw)),
            QRegularExpression::CaseInsensitiveOption);
        rule.format = keywordFormat_;
        rules_.push_back(rule);
    }

    // Numbers.
    rules_.push_back({QRegularExpression(QStringLiteral("\\b[0-9]+(\\.[0-9]+)?\\b")),
                      numberFormat_});
    rules_.push_back(
        {QRegularExpression(QStringLiteral("'([^']|'')*'")), stringFormat_});
    // Double-quoted identifiers styled as strings is close enough here.
    rules_.push_back(
        {QRegularExpression(QStringLiteral("\"([^\"]|\"\")*\"")), stringFormat_});
    // Line comments.
    rules_.push_back(
        {QRegularExpression(QStringLiteral("--[^\n]*")), commentFormat_});

    blockCommentStart_ = QRegularExpression(QStringLiteral("/\\*"));
    blockCommentEnd_ = QRegularExpression(QStringLiteral("\\*/"));
}

void SqlHighlighter::highlightBlock(const QString& text) {
    for (const Rule& rule : rules_) {
        QRegularExpressionMatchIterator it = rule.pattern.globalMatch(text);
        while (it.hasNext()) {
            const QRegularExpressionMatch m = it.next();
            setFormat(static_cast<int>(m.capturedStart()),
                      static_cast<int>(m.capturedLength()), rule.format);
        }
    }

    // Multi-line /* ... */ comments, tracked across blocks via block state.
    setCurrentBlockState(0);
    int startIndex = 0;
    if (previousBlockState() != 1) {
        startIndex = static_cast<int>(text.indexOf(blockCommentStart_));
    }
    while (startIndex >= 0) {
        const QRegularExpressionMatch endMatch =
            blockCommentEnd_.match(text, startIndex);
        const int endIndex = static_cast<int>(endMatch.capturedStart());
        int commentLength = 0;
        if (endIndex < 0) {
            setCurrentBlockState(1);
            commentLength = text.length() - startIndex;
        } else {
            commentLength =
                endIndex - startIndex + static_cast<int>(endMatch.capturedLength());
        }
        setFormat(startIndex, commentLength, commentFormat_);
        startIndex = static_cast<int>(
            text.indexOf(blockCommentStart_, startIndex + commentLength));
    }
}
