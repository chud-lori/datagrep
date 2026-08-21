// ConnectionDialog.hpp — add / edit one connection profile.
//
// The Linux analogue of the macOS ConnectionEditor (New + Edit share one form
// so the two dialogs cannot drift). It drives the profile half of the C ABI
// through the dg::Core wrapper — and ONLY through it:
//
//   Add   -> datagrep_profiles_add_json (name, url-with-password, options-json)
//   Edit  -> datagrep_profiles_update   (original-name, patch-json of ONLY the
//            keys that changed — a full-object write would round-trip and reset
//            fields this build does not understand)
//   Seed  -> datagrep_profiles_get_json (the parsed, secretless config the Edit
//            form populates from; the secret VALUE never crosses this ABI)
//   Info  -> datagrep_connection_info_json (which read-only protection is really
//            in force — server / client / none — shown honestly, never worded up)
//   Test  -> datagrep_connection_test_json (dial once, report what answered,
//            save nothing; runs off the GUI thread — it blocks for the timeout)
//
// The structured fields (host / port / database / user / password) and the URL
// are the SAME value: the URL is rendered from the fields and typing one parses
// straight back, exactly like the macOS ConnectionForm, so the two can never
// disagree. The password lives only behind a masked field and is spliced into
// the URL solely on the one path that hands it to the engine, which lifts it
// into the keychain before anything is written — it is never shown in the URL.
//
// UI glue only: no schema/engine logic beyond building and parsing a URL string,
// which is presentation, not business rules.

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
    // `core` is borrowed and must outlive the dialog. Construct with the two
    // named constructors below rather than directly.
    explicit ConnectionDialog(dg::Core* core, QWidget* parent = nullptr);

    // A blank form for a new connection (datagrep_profiles_add_json on accept).
    static ConnectionDialog* forNewConnection(dg::Core* core, QWidget* parent);

    // An Edit form seeded from datagrep_profiles_get_json for `name`
    // (datagrep_profiles_update with a minimal patch on accept). If the seed
    // read fails the dialog still opens, explaining why, rather than refusing.
    static ConnectionDialog* forEditing(dg::Core* core, const QString& name,
                                        QWidget* parent);

    // The profile name that was added/saved — valid after exec() returns
    // QDialog::Accepted, so the caller can reselect it in the list.
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
    // One engine, as the form needs to describe it (ported subset of the macOS
    // ConnectionEngine table — kept in step with the driver registry).
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

    // The structured connection, the way a person thinks about it. The URL and
    // these fields are the same value rendered two ways.
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

    // URL <-> fields, ported from the macOS ConnectionURL so New and Edit agree
    // with the CLI on the profile's storage format.
    QString buildUrl(const Fields& f, bool includePassword) const;
    Fields parseUrl(const QString& url) const;
    Fields fieldsFromConfig(const QString& driver, const class QJsonObject& config) const;

    dg::Core* core_;
    bool editing_ = false;
    QString originalName_;
    QString savedName_;

    // The Edit form's baseline, so the patch carries only what actually moved.
    // Populated from datagrep_profiles_get_json.
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
