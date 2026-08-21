#ifndef DATAGREP_SQL_HIGHLIGHTER_HPP
#define DATAGREP_SQL_HIGHLIGHTER_HPP

#include <QRegularExpression>
#include <QSyntaxHighlighter>
#include <QTextCharFormat>
#include <QVector>

class SqlHighlighter : public QSyntaxHighlighter {
    Q_OBJECT

public:
    explicit SqlHighlighter(QTextDocument* document);

protected:
    void highlightBlock(const QString& text) override;

private:
    struct Rule {
        QRegularExpression pattern;
        QTextCharFormat format;
    };
    QVector<Rule> rules_;
    QTextCharFormat keywordFormat_;
    QTextCharFormat stringFormat_;
    QTextCharFormat numberFormat_;
    QTextCharFormat commentFormat_;
    QRegularExpression blockCommentStart_;
    QRegularExpression blockCommentEnd_;
};

#endif  // DATAGREP_SQL_HIGHLIGHTER_HPP
