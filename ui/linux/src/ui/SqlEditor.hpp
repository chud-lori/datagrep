// SqlEditor.hpp — the SQL editing pane.

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

    QString allText() const;

    QString statementUnderCursor() const;

signals:
    // Ctrl+Return: "run this".
    void runRequested();

protected:
    void keyPressEvent(QKeyEvent* event) override;
    // Palette (light/dark) changes must retint the KSyntax theme.
    void changeEvent(QEvent* event) override;

private:
    // Re-select the KSyntax theme for the current palette; no-op in fallback.
    void applyTheme();

    // Owned by the document (QSyntaxHighlighter parents itself to it).
    QSyntaxHighlighter* highlighter_ = nullptr;

#ifdef HAVE_KSYNTAXHIGHLIGHTING
    KSyntaxHighlighting::Repository* repository_ = nullptr;
    bool applyingTheme_ = false;  // guards the setPalette() re-entry in applyTheme()
#endif
};

#endif  // DATAGREP_SQL_EDITOR_HPP
