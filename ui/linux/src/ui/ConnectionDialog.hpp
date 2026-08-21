#ifndef DATAGREP_CONNECTION_DIALOG_HPP
#define DATAGREP_CONNECTION_DIALOG_HPP

#include <QDialog>
#include <QFutureWatcher>
#include <QPair>
#include <QString>
#include <QStringList>

namespace dg {
class Core;
}
class QLineEdit;
class QComboBox;
class QCheckBox;
class QLabel;
class QPushButton;
class QDialogButtonBox;

class ConnectionDialog : public QDialog {
    Q_OBJECT

public:
    // `core` is borrowed and must outlive the dialog; construct via the factories.
    explicit ConnectionDialog(dg::Core* core, QWidget* parent = nullptr);

    static ConnectionDialog* forNewConnection(dg::Core* core, QWidget* parent);

    // Seeded from datagrep_profiles_get_json; a failed seed still opens, explaining why.
    static ConnectionDialog* forEditing(dg::Core* core, const QString& name,
                                        QWidget* parent);

    // The name added/saved — valid once exec() returns QDialog::Accepted.
    QString savedName() const { return savedName_; }

private slots:
    void onEngineChanged(int index);
    void onFieldEdited();     // a structured field changed -> re-render the URL
    void onUrlEdited();       // the URL box was typed in -> parse back to fields
    void onReadOnlyToggled(bool on);
    void onBrowseFile();
    void onCheckEnforcement();  // datagrep_connection_info_json for this profile
    void onTestConnection();    // datagrep_connection_test_json, off the GUI thread
    void onTestFinished();
    void onAccept();

private:
    // One engine as the form describes it — kept in step with the driver registry.
    struct Engine {
        QString id;
        QString scheme;       // canonical scheme, e.g. "postgres://"
        QStringList aliases;  // other spellings a pasted URL may use
        QString tlsScheme;    // scheme when TLS is on, or empty
        int defaultPort;      // -1 for none
        bool fileBased;
        QString databaseLabel;
        QString databasePlaceholder;
    };

    // The structured connection; the URL and these fields are one value, two renderings.
    struct Fields {
        QString engineId = QStringLiteral("postgres");
        QString host, port, database, username, password, filePath, extras;
        bool tls = false;
    };

    void buildUi();
    void showError(const QString& text);  // sets + shows; hidden while empty
    void seedForEdit(const QString& name);
    void applyFieldsToUi(const Fields& f);   // fields -> widgets (no URL round-trip)
    Fields fieldsFromUi() const;             // widgets -> fields
    void renderUrlFromFields();              // fields -> URL box
    void reshapeForEngine(const Engine& e);  // show file vs host/port, TLS, labels
    QString optionsJson() const;             // add: full options object
    QString patchJson() const;               // edit: only the changed keys

    const Engine* engineById(const QString& id) const;
    const Engine* currentEngine() const;

    // URL <-> fields, ported from the macOS ConnectionURL so all builds agree.
    QString buildUrl(const Fields& f, bool includePassword) const;
    Fields parseUrl(const QString& url) const;
    Fields fieldsFromConfig(const QString& driver, const class QJsonObject& config) const;

    dg::Core* core_;
    bool editing_ = false;
    QString originalName_;
    QString savedName_;

    // The Edit form's baseline, so the patch carries only what actually moved.
    bool haveOriginal_ = false;
    QString originalUrlNoPassword_;
    bool origReadOnly_ = false;
    bool origConfirmWrites_ = false;
    QString origColor_;
    QString origAutoLimit_;    // as text; empty == unset
    QString origIdleTimeout_;  // as text; empty == unset

    // Guards the fields<->URL two-way sync against recursive updates.
    bool syncing_ = false;

    // --- widgets ---
    QComboBox* engineBox_;
    QLineEdit* nameEdit_;
    QLabel* hostLabel_;
    QLineEdit* hostEdit_;
    QLabel* portLabel_;
    QLineEdit* portEdit_;
    QLabel* databaseLabel_;
    QLineEdit* databaseEdit_;
    QLabel* usernameLabel_;
    QLineEdit* usernameEdit_;
    QLabel* passwordLabel_;
    QLineEdit* passwordEdit_;
    QLabel* passwordHint_;
    QLabel* fileLabel_;
    QLineEdit* fileEdit_;
    QPushButton* browseButton_;
    QCheckBox* tlsCheck_;
    QLineEdit* urlEdit_;

    QComboBox* colorBox_;
    QCheckBox* readOnlyCheck_;
    QCheckBox* confirmWritesCheck_;
    QLineEdit* autoLimitEdit_;
    QLineEdit* idleTimeoutEdit_;

    QPushButton* testButton_;
    QLabel* testResultLabel_;
    // first = result JSON, second = the driver's failure message
    QFutureWatcher<QPair<QString, QString>>* testWatcher_;

    QPushButton* enforcementButton_;
    QLabel* enforcementLabel_;
    QLabel* errorLabel_;
    QDialogButtonBox* buttons_;
};

#endif  // DATAGREP_CONNECTION_DIALOG_HPP
