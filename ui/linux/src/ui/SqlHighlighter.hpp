// SqlHighlighter.hpp — a minimal SQL QSyntaxHighlighter for the editor.
//
// This is the compile-time FALLBACK highlighter. It is a plain QSyntaxHighlighter
// (no external dependency) so ui/linux builds and highlights with only Qt6
// present.
//
// The preferred path is KSyntaxHighlighting (KF6, MIT-licensed): it ships a
// KSyntaxHighlighting::SyntaxHighlighter (a QSyntaxHighlighter subclass) plus a
// maintained SQL syntax definition and light/dark themes. When CMake finds
// KF6SyntaxHighlighting it defines HAVE_KSYNTAXHIGHLIGHTING and SqlEditor drives
// that engine directly (see SqlEditor.cpp); this class is compiled and attached
// only when the package is absent. The editor talks to whichever highlighter it
// uses through the QSyntaxHighlighter base alone, so the two are interchangeable.
//
// (QScintilla was considered for a full editor widget with folding/autocomplete
// but is GPLv3 / commercial only, which would impose GPLv3 on the whole UI
// binary, so it is intentionally NOT wired here.)

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
