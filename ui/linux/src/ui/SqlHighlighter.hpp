// SqlHighlighter.hpp — a minimal SQL QSyntaxHighlighter for the editor.
//
// This is the "basic highlighter is fine for now" placeholder. It is a plain
// QSyntaxHighlighter (no external dependency) so the skeleton builds with only
// Qt6 present.
//
// UPGRADE SEAM — the editor deliberately talks to its highlighter only through
// QSyntaxHighlighter, so this class can be swapped without touching SqlEditor:
//
//   * Preferred: KSyntaxHighlighting (KF6, MIT-licensed). It ships a
//     KSyntaxHighlighting::SyntaxHighlighter (a QSyntaxHighlighter subclass) plus
//     a maintained SQL syntax definition and theme support. Add
//     `find_package(KF6SyntaxHighlighting)` and construct it against a
//     QPlainTextEdit document — a near drop-in for this class.
//
//   * QScintilla (ScintillaEdit) gives a full editor widget with folding and
//     autocomplete, but it is GPLv3 / commercial only. Adopting it would impose
//     GPLv3 on the whole UI binary, so it is intentionally NOT wired here; if the
//     project accepts that licensing, replace the QPlainTextEdit in SqlEditor
//     with a QsciScintilla and drop this highlighter entirely.

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
