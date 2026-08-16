// SqlEditor.hpp — the SQL editing pane.
//
// A QPlainTextEdit with a syntax highlighter attached to its QTextDocument. The
// widget talks to its highlighter only through the QSyntaxHighlighter base, so
// what actually colours the text is decided at build time:
//
//   * When KF6SyntaxHighlighting is available (HAVE_KSYNTAXHIGHLIGHTING), the
//     editor drives KSyntaxHighlighting::SyntaxHighlighter with the maintained
//     "SQL" definition and a theme chosen to match the widget palette (light or
//     dark). This is the preferred, MIT-licensed path.
//
//   * Otherwise it falls back to the built-in SqlHighlighter (a plain
//     QSyntaxHighlighter, no external dependency) so the build never breaks when
//     the KF6 package is absent.
//
// statementUnderCursor() implements "run the statement under the cursor": the
// text is split into statements on ';' boundaries that are NOT inside a string
// literal or a comment, and the statement containing the cursor is returned.
// Exactly that substring is what MainWindow passes to datagrep_query_run.

#ifndef DATAGREP_SQL_EDITOR_HPP
#define DATAGREP_SQL_EDITOR_HPP

#include <QPlainTextEdit>
#include <QSyntaxHighlighter>

#ifdef HAVE_KSYNTAXHIGHLIGHTING
namespace KSyntaxHighlighting {
class Repository;
}
#endif

class SqlEditor : public QPlainTextEdit {
    Q_OBJECT

public:
    explicit SqlEditor(QWidget* parent = nullptr);

    // The whole buffer, trimmed.
    QString allText() const;

    // The statement the cursor sits in, split on top-level ';' (semicolons inside
    // '…' strings, "…" identifiers, -- line comments and /* … */ block comments
    // are ignored). Falls back to the whole buffer if splitting finds nothing.
    QString statementUnderCursor() const;

signals:
    // Emitted on Ctrl+Return / Cmd+Return: "run this".
    void runRequested();

protected:
    void keyPressEvent(QKeyEvent* event) override;
    // React to palette (light/dark) changes so the KSyntax theme tracks them.
    void changeEvent(QEvent* event) override;

private:
    // Re-select the KSyntax theme for the current palette and repaint. A no-op
    // in the fallback build.
    void applyTheme();

    // Owned by the document (QSyntaxHighlighter parents itself to it). Held as
    // the base type; the concrete highlighter depends on the build.
    QSyntaxHighlighter* highlighter_ = nullptr;

#ifdef HAVE_KSYNTAXHIGHLIGHTING
    KSyntaxHighlighting::Repository* repository_ = nullptr;
    bool applyingTheme_ = false;  // guards the setPalette() re-entry in applyTheme()
#endif
};

#endif  // DATAGREP_SQL_EDITOR_HPP
