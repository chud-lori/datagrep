#include "ConnectionDialog.hpp"

#include "ffi/DatagrepFfi.hpp"
#include "ui/EngineIcon.hpp"

#include <QCheckBox>
#include <QComboBox>
#include <QDialogButtonBox>
#include <QFileDialog>
#include <QFont>
#include <QFormLayout>
#include <QFrame>
#include <QHBoxLayout>
#include <QGroupBox>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QJsonValue>
#include <QLabel>
#include <QLineEdit>
#include <QPushButton>
#include <QUrl>
#include <QVBoxLayout>
#include <QtConcurrent/QtConcurrent>

// ---------------------------------------------------------------------------
// Engine table — the ported subset of the macOS ConnectionEngines list. Kept in
// step with crates/datagrep-ffi/src/drivers.rs: an engine offered here that the
// build cannot route a URL for would fail on Add, which is worse than not
// offering it. Static, so it needs no per-dialog allocation.
// ---------------------------------------------------------------------------
namespace {

QString percentEncode(const QString& s) {
    // Unreserved set (A-Za-z0-9-._~) — exactly what the macOS encoder allows, so
    // a URL built here round-trips through the CLI unchanged.
    return QString::fromUtf8(QUrl::toPercentEncoding(s));
}

QString percentDecode(const QString& s) {
    return QUrl::fromPercentEncoding(s.toUtf8());
}

}  // namespace

const ConnectionDialog::Engine* ConnectionDialog::engineById(const QString& id) const {
    // A tiny static registry; folded spellings (mongodb->mongo, mariadb->mysql)
    // are handled by matching id prefixes below.
    static const Engine engines[] = {
        {QStringLiteral("postgres"), QStringLiteral("postgres://"),
         {QStringLiteral("postgresql://")}, QString(), 5432, false,
         QStringLiteral("Database"), QStringLiteral("mydb")},
        {QStringLiteral("mysql"), QStringLiteral("mysql://"),
         {QStringLiteral("mariadb://")}, QString(), 3306, false,
         QStringLiteral("Database"), QStringLiteral("mydb")},
        {QStringLiteral("sqlite"), QStringLiteral("sqlite://"), {}, QString(), -1,
         true, QStringLiteral("File"), QStringLiteral("/home/me/data.db")},
        {QStringLiteral("redis"), QStringLiteral("redis://"),
         {QStringLiteral("rediss://")}, QString(), 6379, false,
         QStringLiteral("Database index"), QStringLiteral("0")},
        {QStringLiteral("mongo"), QStringLiteral("mongodb://"),
         {QStringLiteral("mongodb+srv://")}, QString(), 27017, false,
         QStringLiteral("Database"), QStringLiteral("mydb")},
        {QStringLiteral("elasticsearch"), QStringLiteral("http://"),
         {QStringLiteral("elasticsearch://")}, QStringLiteral("https://"), 9200,
         false, QStringLiteral("Default index"), QStringLiteral("optional")},
    };

    // The shared folding table, so a profile's stored driver ("postgresql",
    // "mariadb", "mongodb") still matches — one definition of what an engine
    // is, same as EngineStyle.canonicalID keeps on macOS.
    const QString key = dg::canonicalDriverId(id);
    for (const Engine& e : engines) {
        if (e.id == key) {
            return &e;
        }
    }
    return nullptr;
}

const ConnectionDialog::Engine* ConnectionDialog::currentEngine() const {
    return engineById(engineBox_->currentData().toString());
}

// ---------------------------------------------------------------------------
// URL <-> fields. Ported from DatagrepKit.ConnectionURL so New and Edit agree
// with the CLI on the profile's storage format, and pasting a URL fills the
// fields while editing the fields rewrites the URL.
// ---------------------------------------------------------------------------

QString ConnectionDialog::buildUrl(const Fields& f, bool includePassword) const {
    const Engine* e = engineById(f.engineId);
    if (e == nullptr) {
        return QString();
    }

    if (e->fileBased) {
        const QString path = f.filePath.trimmed();
        if (path.isEmpty()) {
            return QString();
        }
        if (path == QStringLiteral(":memory:")) {
            return path;
        }
        // sqlite:// + an absolute path is sqlite:///home/… — three slashes; the
        // driver takes everything after the second as the path.
        return e->scheme + (path.startsWith(QLatin1Char('/')) ? path
                                                              : QStringLiteral("/") + path);
    }

    const QString host = f.host.trimmed();
    if (host.isEmpty()) {
        return QString();
    }
    QString out = (f.tls && !e->tlsScheme.isEmpty()) ? e->tlsScheme : e->scheme;

    const QString user = f.username.trimmed();
    if (!user.isEmpty()) {
        out += percentEncode(user);
        if (includePassword && !f.password.isEmpty()) {
            out += QLatin1Char(':') + percentEncode(f.password);
        }
        out += QLatin1Char('@');
    }
    // An IPv6 literal keeps its brackets, or the ':' before the port reads as
    // part of the address.
    if (host.contains(QLatin1Char(':')) && !host.startsWith(QLatin1Char('['))) {
        out += QLatin1Char('[') + host + QLatin1Char(']');
    } else {
        out += host;
    }
    bool portOk = false;
    const int typedPort = f.port.trimmed().toInt(&portOk);
    const int port = portOk ? typedPort : e->defaultPort;
    if (port >= 0) {
        out += QLatin1Char(':') + QString::number(port);
    }
    const QString db = f.database.trimmed();
    if (!db.isEmpty()) {
        out += QLatin1Char('/') + db;
    }
    const QString extras = f.extras.trimmed();
    if (!extras.isEmpty()) {
        out += QLatin1Char('?') + extras;
    }
    return out;
}

ConnectionDialog::Fields ConnectionDialog::parseUrl(const QString& url) const {
    Fields f;
    f.engineId.clear();  // empty == unknown scheme; the caller keeps its engine
    const QString trimmed = url.trimmed();
    const QString lower = trimmed.toLower();

    if (lower == QStringLiteral(":memory:")) {
        f.engineId = QStringLiteral("sqlite");
        f.filePath = QStringLiteral(":memory:");
        return f;
    }

    // Scheme -> engine, by matching any spelling the engine accepts.
    const Engine* engine = nullptr;
    const QString ids[] = {
        QStringLiteral("postgres"),      QStringLiteral("mysql"),
        QStringLiteral("sqlite"),        QStringLiteral("redis"),
        QStringLiteral("mongo"),         QStringLiteral("elasticsearch")};
    for (const QString& id : ids) {
        const Engine* e = engineById(id);
        if (e == nullptr) {
            continue;
        }
        QStringList schemes = e->aliases;
        schemes.prepend(e->scheme);
        if (!e->tlsScheme.isEmpty()) {
            schemes.append(e->tlsScheme);
        }
        for (const QString& sc : schemes) {
            if (lower.startsWith(sc)) {
                engine = e;
                break;
            }
        }
        if (engine != nullptr) {
            break;
        }
    }
    if (engine == nullptr) {
        return f;  // still half-typed / unknown
    }
    f.engineId = engine->id;

    const int schemeEnd = trimmed.indexOf(QStringLiteral("://"));
    if (schemeEnd < 0) {
        return f;
    }
    const QString scheme = trimmed.left(schemeEnd).toLower() + QStringLiteral("://");
    f.tls = !engine->tlsScheme.isEmpty() && scheme == engine->tlsScheme;
    QString rest = trimmed.mid(schemeEnd + 3);

    if (engine->fileBased) {
        f.filePath = rest;
        return f;
    }

    const int q = rest.indexOf(QLatin1Char('?'));
    if (q >= 0) {
        f.extras = rest.mid(q + 1);
        rest = rest.left(q);
    }
    // firstIndex of '/', so an Elasticsearch proxy prefix that itself contains a
    // slash is kept whole rather than cut at the second one.
    const int slash = rest.indexOf(QLatin1Char('/'));
    if (slash >= 0) {
        f.database = percentDecode(rest.mid(slash + 1));
        rest = rest.left(slash);
    }
    // lastIndex of '@': a password may legally contain one.
    const int at = rest.lastIndexOf(QLatin1Char('@'));
    if (at >= 0) {
        const QString userinfo = rest.left(at);
        rest = rest.mid(at + 1);
        const int colon = userinfo.indexOf(QLatin1Char(':'));
        if (colon >= 0) {
            f.username = percentDecode(userinfo.left(colon));
            f.password = percentDecode(userinfo.mid(colon + 1));
        } else {
            f.username = percentDecode(userinfo);
        }
    }
    if (rest.startsWith(QLatin1Char('['))) {
        const int close = rest.indexOf(QLatin1Char(']'));
        if (close >= 0) {
            f.host = rest.mid(1, close - 1);
            const QString tail = rest.mid(close + 1);
            if (tail.startsWith(QLatin1Char(':'))) {
                f.port = tail.mid(1);
            }
        }
    } else {
        const int colon = rest.lastIndexOf(QLatin1Char(':'));
        if (colon >= 0) {
            f.host = rest.left(colon);
            f.port = rest.mid(colon + 1);
        } else {
            f.host = rest;
        }
    }
    return f;
}

ConnectionDialog::Fields ConnectionDialog::fieldsFromConfig(const QString& driver,
                                                            const QJsonObject& config) const {
    Fields f;
    const Engine* e = engineById(driver);
    if (e == nullptr) {
        return f;
    }
    f.engineId = e->id;

    auto str = [&](const QString& key) -> QString {
        const QJsonValue v = config.value(key);
        if (!v.isString()) {
            return QString();
        }
        const QString s = v.toString();
        // The ABI masks a stored secret to "••••"; it must never be pasted into
        // a URL.
        return (s.isEmpty() || s == QString::fromUtf8("••••"))
                   ? QString()
                   : s;
    };
    auto num = [&](const QString& key) -> QString {
        const QJsonValue v = config.value(key);
        if (v.isDouble()) {
            return QString::number(static_cast<qlonglong>(v.toDouble()));
        }
        return str(key);
    };
    auto flag = [&](const QString& key) -> bool {
        const QJsonValue v = config.value(key);
        if (v.isBool()) {
            return v.toBool();
        }
        const QString s = str(key);
        return s == QStringLiteral("true") || s == QStringLiteral("require");
    };

    if (e->fileBased) {
        f.filePath = str(QStringLiteral("path"));
        return f;
    }
    f.host = str(QStringLiteral("host"));
    if (f.host.isEmpty()) {
        const QString hosts = str(QStringLiteral("hosts"));
        if (!hosts.isEmpty()) {
            f.host = hosts.split(QLatin1Char(',')).first().trimmed();
        }
    }
    f.port = num(QStringLiteral("port"));
    f.username = str(QStringLiteral("user"));
    if (f.username.isEmpty()) {
        f.username = str(QStringLiteral("username"));
    }
    f.database = str(QStringLiteral("database"));
    if (f.database.isEmpty()) {
        // Redis stores its db index as a JSON number.
        f.database = num(QStringLiteral("db"));
    }
    if (f.database.isEmpty()) {
        f.database = str(QStringLiteral("index"));
    }
    if (!e->tlsScheme.isEmpty()) {
        f.tls = flag(QStringLiteral("tls"));
    }
    return f;
}

// ---------------------------------------------------------------------------
// Construction / factory
// ---------------------------------------------------------------------------

ConnectionDialog::ConnectionDialog(dg::Core* core, QWidget* parent)
    : QDialog(parent), core_(core) {
    buildUi();
}

ConnectionDialog* ConnectionDialog::forNewConnection(dg::Core* core, QWidget* parent) {
    auto* d = new ConnectionDialog(core, parent);
    d->setWindowTitle(QStringLiteral("New Connection"));
    d->editing_ = false;
    d->buttons_->button(QDialogButtonBox::Save)->setText(QStringLiteral("Add"));
    d->enforcementButton_->hide();  // nothing to check until the profile exists
    d->onEngineChanged(d->engineBox_->currentIndex());  // shape + first URL
    return d;
}

ConnectionDialog* ConnectionDialog::forEditing(dg::Core* core, const QString& name,
                                               QWidget* parent) {
    auto* d = new ConnectionDialog(core, parent);
    d->setWindowTitle(QStringLiteral("Edit Connection"));
    d->editing_ = true;
    d->originalName_ = name;
    d->buttons_->button(QDialogButtonBox::Save)->setText(QStringLiteral("Save"));
    d->seedForEdit(name);
    return d;
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

void ConnectionDialog::buildUi() {
    auto* outer = new QVBoxLayout(this);
    outer->setSpacing(10);

    // --- engine + connection fields ---------------------------------------
    engineBox_ = new QComboBox(this);
    const QString ids[] = {
        QStringLiteral("postgres"),      QStringLiteral("mysql"),
        QStringLiteral("sqlite"),        QStringLiteral("redis"),
        QStringLiteral("mongo"),         QStringLiteral("elasticsearch")};
    for (const QString& id : ids) {
        engineBox_->addItem(dg::engineIcon(id), dg::engineDisplayName(id), id);
    }
    connect(engineBox_, QOverload<int>::of(&QComboBox::currentIndexChanged), this,
            &ConnectionDialog::onEngineChanged);

    nameEdit_ = new QLineEdit(this);
    nameEdit_->setPlaceholderText(QStringLiteral("a name for this connection"));

    hostEdit_ = new QLineEdit(this);
    hostEdit_->setPlaceholderText(QStringLiteral("localhost"));
    portEdit_ = new QLineEdit(this);
    portEdit_->setPlaceholderText(QStringLiteral("default"));
    portEdit_->setMaximumWidth(90);
    databaseEdit_ = new QLineEdit(this);
    usernameEdit_ = new QLineEdit(this);
    passwordEdit_ = new QLineEdit(this);
    passwordEdit_->setEchoMode(QLineEdit::Password);
    passwordHint_ = new QLabel(this);
    passwordHint_->setWordWrap(true);
    passwordHint_->setStyleSheet(QStringLiteral("color: gray; font-size: 11px;"));
    passwordHint_->setText(QStringLiteral(
        "Moved into the system keychain before the connection is written; it "
        "never reaches disk in plain text and is never shown in the URL below."));

    fileEdit_ = new QLineEdit(this);
    browseButton_ = new QPushButton(QStringLiteral("Choose…"), this);
    connect(browseButton_, &QPushButton::clicked, this,
            &ConnectionDialog::onBrowseFile);

    tlsCheck_ = new QCheckBox(QStringLiteral("Use TLS (https)"), this);

    urlEdit_ = new QLineEdit(this);
    QFont mono = urlEdit_->font();
    mono.setStyleHint(QFont::Monospace);
    mono.setFamily(QStringLiteral("monospace"));
    urlEdit_->setFont(mono);
    urlEdit_->setPlaceholderText(QStringLiteral("postgres://user@localhost:5432/mydb"));

    // Host / port share one row so they line up like every other client.
    auto* hostPort = new QWidget(this);
    auto* hostPortLayout = new QHBoxLayout(hostPort);
    hostPortLayout->setContentsMargins(0, 0, 0, 0);
    portLabel_ = new QLabel(QStringLiteral("Port"), this);
    hostPortLayout->addWidget(hostEdit_, 1);
    hostPortLayout->addWidget(portLabel_);
    hostPortLayout->addWidget(portEdit_);

    auto* fileRow = new QWidget(this);
    auto* fileRowLayout = new QHBoxLayout(fileRow);
    fileRowLayout->setContentsMargins(0, 0, 0, 0);
    fileRowLayout->addWidget(fileEdit_, 1);
    fileRowLayout->addWidget(browseButton_);

    auto* connForm = new QFormLayout();
    // Explicit: the default growth policy is style-dependent.
    connForm->setFieldGrowthPolicy(QFormLayout::AllNonFixedFieldsGrow);
    connForm->addRow(QStringLiteral("Engine"), engineBox_);
    connForm->addRow(QStringLiteral("Name"), nameEdit_);
    hostLabel_ = new QLabel(QStringLiteral("Host"), this);
    connForm->addRow(hostLabel_, hostPort);
    fileLabel_ = new QLabel(QStringLiteral("File"), this);
    connForm->addRow(fileLabel_, fileRow);
    databaseLabel_ = new QLabel(QStringLiteral("Database"), this);
    connForm->addRow(databaseLabel_, databaseEdit_);
    usernameLabel_ = new QLabel(QStringLiteral("Username"), this);
    connForm->addRow(usernameLabel_, usernameEdit_);
    passwordLabel_ = new QLabel(QStringLiteral("Password"), this);
    connForm->addRow(passwordLabel_, passwordEdit_);
    connForm->addRow(QString(), passwordHint_);
    connForm->addRow(QString(), tlsCheck_);
    connForm->addRow(QStringLiteral("Connection URL"), urlEdit_);

    testButton_ = new QPushButton(QStringLiteral("Test Connection"), this);
    testButton_->setToolTip(QStringLiteral(
        "Open one connection with these settings and report what answers. "
        "Nothing is saved by testing."));
    connect(testButton_, &QPushButton::clicked, this,
            &ConnectionDialog::onTestConnection);
    testResultLabel_ = new QLabel(this);
    testResultLabel_->setWordWrap(true);
    testResultLabel_->setTextInteractionFlags(Qt::TextSelectableByMouse);
    testResultLabel_->hide();
    testWatcher_ = new QFutureWatcher<QPair<QString, QString>>(this);
    connect(testWatcher_, &QFutureWatcher<QPair<QString, QString>>::finished, this,
            &ConnectionDialog::onTestFinished);
    auto* testRow = new QHBoxLayout();
    testRow->addWidget(testButton_);
    testRow->addStretch(1);
    connForm->addRow(QString(), testRow);
    connForm->addRow(QString(), testResultLabel_);

    auto* connGroup = new QGroupBox(QStringLiteral("Connection"), this);
    connGroup->setLayout(connForm);
    outer->addWidget(connGroup);

    // Two-way field <-> URL sync. Fields use textChanged (any change re-renders
    // the URL); the URL box uses textEdited (only USER edits re-parse — a
    // programmatic setText must not bounce back).
    for (QLineEdit* e : {hostEdit_, portEdit_, databaseEdit_, usernameEdit_, fileEdit_}) {
        connect(e, &QLineEdit::textChanged, this, &ConnectionDialog::onFieldEdited);
    }
    connect(tlsCheck_, &QCheckBox::toggled, this, &ConnectionDialog::onFieldEdited);
    connect(urlEdit_, &QLineEdit::textEdited, this, &ConnectionDialog::onUrlEdited);

    // --- settings ----------------------------------------------------------
    // No environment picker: the engine dropped the dev/staging/prod enum in
    // favour of the colour marker and the read-only tick, and still sending
    // `env` made every save from this dialog fail.

    colorBox_ = new QComboBox(this);
    colorBox_->addItem(QStringLiteral("none"), QString());
    for (const QString& c :
         {QStringLiteral("red"), QStringLiteral("orange"), QStringLiteral("yellow"),
          QStringLiteral("green"), QStringLiteral("blue"), QStringLiteral("purple"),
          QStringLiteral("graphite")}) {
        colorBox_->addItem(c, c);
    }

    autoLimitEdit_ = new QLineEdit(this);
    autoLimitEdit_->setPlaceholderText(QStringLiteral("none"));
    autoLimitEdit_->setMaximumWidth(120);
    autoLimitEdit_->setToolTip(
        QStringLiteral("rows fetched before datagrep stops on its own"));
    idleTimeoutEdit_ = new QLineEdit(this);
    idleTimeoutEdit_->setPlaceholderText(QStringLiteral("none"));
    idleTimeoutEdit_->setMaximumWidth(120);
    idleTimeoutEdit_->setToolTip(
        QStringLiteral("seconds before an unused connection is dropped"));

    auto* settingsForm = new QFormLayout();
    settingsForm->setFieldGrowthPolicy(QFormLayout::AllNonFixedFieldsGrow);
    settingsForm->addRow(QStringLiteral("Colour"), colorBox_);
    settingsForm->addRow(QStringLiteral("Row limit"), autoLimitEdit_);
    settingsForm->addRow(QStringLiteral("Idle timeout (s)"), idleTimeoutEdit_);

    auto* settingsGroup = new QGroupBox(QStringLiteral("Settings"), this);
    settingsGroup->setLayout(settingsForm);

    // --- safety ------------------------------------------------------------
    readOnlyCheck_ = new QCheckBox(QStringLiteral("Read-only"), this);
    readOnlyCheck_->setToolTip(QStringLiteral(
        "Refuse writes on this connection even when the account is allowed to "
        "write."));
    confirmWritesCheck_ = new QCheckBox(QStringLiteral("Ask before running a write"),
                                        this);
    confirmWritesCheck_->setToolTip(
        QStringLiteral("Show a confirmation before INSERT / UPDATE / DELETE / DROP."));
    connect(readOnlyCheck_, &QCheckBox::toggled, this,
            &ConnectionDialog::onReadOnlyToggled);

    enforcementButton_ = new QPushButton(QStringLiteral("Check read-only enforcement"),
                                         this);
    enforcementButton_->setToolTip(QStringLiteral(
        "Ask the engine which protection is actually in force — server, client, "
        "or none."));
    connect(enforcementButton_, &QPushButton::clicked, this,
            &ConnectionDialog::onCheckEnforcement);
    enforcementLabel_ = new QLabel(this);
    enforcementLabel_->setWordWrap(true);
    enforcementLabel_->setTextInteractionFlags(Qt::TextSelectableByMouse);
    enforcementLabel_->hide();  // shown once there is an answer to report

    auto* safetyLayout = new QVBoxLayout();
    safetyLayout->addWidget(readOnlyCheck_);
    auto* roHint = new QLabel(
        QStringLiteral("Refuses writes on this connection even when the database "
                       "account is allowed to make them."),
        this);
    roHint->setWordWrap(true);
    roHint->setStyleSheet(QStringLiteral("color: gray; font-size: 11px;"));
    safetyLayout->addWidget(roHint);
    safetyLayout->addWidget(confirmWritesCheck_);
    auto* enforcementRow = new QHBoxLayout();
    enforcementRow->addWidget(enforcementButton_);
    enforcementRow->addStretch(1);
    safetyLayout->addLayout(enforcementRow);
    safetyLayout->addWidget(enforcementLabel_);
    safetyLayout->addStretch(1);

    auto* safetyGroup = new QGroupBox(QStringLiteral("Safety"), this);
    safetyGroup->setLayout(safetyLayout);

    auto* midRow = new QHBoxLayout();
    midRow->addWidget(settingsGroup, 1);
    midRow->addWidget(safetyGroup, 1);
    outer->addLayout(midRow);

    // --- errors + buttons --------------------------------------------------
    errorLabel_ = new QLabel(this);
    errorLabel_->setWordWrap(true);
    errorLabel_->setTextInteractionFlags(Qt::TextSelectableByMouse);
    errorLabel_->setStyleSheet(QStringLiteral("color: #c0392b;"));
    errorLabel_->hide();
    outer->addWidget(errorLabel_);

    buttons_ = new QDialogButtonBox(QDialogButtonBox::Save | QDialogButtonBox::Cancel,
                                    this);
    connect(buttons_, &QDialogButtonBox::accepted, this, &ConnectionDialog::onAccept);
    connect(buttons_, &QDialogButtonBox::rejected, this, &QDialog::reject);
    outer->addWidget(buttons_);

    if (core_ == nullptr) {
        showError(QStringLiteral(
            "The datagrep engine is not available, so connections cannot be saved."));
        buttons_->button(QDialogButtonBox::Save)->setEnabled(false);
        testButton_->setEnabled(false);
    }

    // Visual order, not construction order (the File row sits above Database).
    QWidget::setTabOrder(engineBox_, nameEdit_);
    QWidget::setTabOrder(nameEdit_, hostEdit_);
    QWidget::setTabOrder(hostEdit_, portEdit_);
    QWidget::setTabOrder(portEdit_, fileEdit_);
    QWidget::setTabOrder(fileEdit_, browseButton_);
    QWidget::setTabOrder(browseButton_, databaseEdit_);
    QWidget::setTabOrder(databaseEdit_, usernameEdit_);
    QWidget::setTabOrder(usernameEdit_, passwordEdit_);
    QWidget::setTabOrder(passwordEdit_, tlsCheck_);
    QWidget::setTabOrder(tlsCheck_, urlEdit_);
    QWidget::setTabOrder(urlEdit_, testButton_);
    QWidget::setTabOrder(testButton_, colorBox_);
    QWidget::setTabOrder(colorBox_, autoLimitEdit_);
    QWidget::setTabOrder(autoLimitEdit_, idleTimeoutEdit_);
    QWidget::setTabOrder(idleTimeoutEdit_, readOnlyCheck_);
    QWidget::setTabOrder(readOnlyCheck_, confirmWritesCheck_);
    QWidget::setTabOrder(confirmWritesCheck_, enforcementButton_);

    setMinimumWidth(560);
    setSizeGripEnabled(true);
    onReadOnlyToggled(false);
}

void ConnectionDialog::showError(const QString& text) {
    errorLabel_->setText(text);
    errorLabel_->setVisible(!text.isEmpty());
}

void ConnectionDialog::reshapeForEngine(const Engine& e) {
    const bool file = e.fileBased;
    hostLabel_->setVisible(!file);
    hostEdit_->setVisible(!file);
    portLabel_->setVisible(!file);
    portEdit_->setVisible(!file);
    usernameLabel_->setVisible(!file);
    usernameEdit_->setVisible(!file);
    passwordLabel_->setVisible(!file);
    passwordEdit_->setVisible(!file);
    passwordHint_->setVisible(!file);
    // parent widget of the host/port row shares the host label's visibility.
    hostEdit_->parentWidget()->setVisible(!file);

    fileLabel_->setVisible(file);
    fileEdit_->parentWidget()->setVisible(file);

    databaseLabel_->setText(e.databaseLabel);
    databaseEdit_->setPlaceholderText(e.databasePlaceholder);
    if (!file) {
        portEdit_->setPlaceholderText(
            e.defaultPort >= 0 ? QString::number(e.defaultPort)
                               : QStringLiteral("default"));
    }
    tlsCheck_->setVisible(!e.tlsScheme.isEmpty());
    if (e.tlsScheme.isEmpty()) {
        tlsCheck_->setChecked(false);
    }
}

// ---------------------------------------------------------------------------
// Field <-> UI helpers
// ---------------------------------------------------------------------------

void ConnectionDialog::applyFieldsToUi(const Fields& f) {
    // Caller sets syncing_ so these setText calls do not bounce back through the
    // URL parser or re-render the URL mid-edit.
    if (!f.engineId.isEmpty()) {
        const int idx = engineBox_->findData(f.engineId);
        if (idx >= 0) {
            engineBox_->setCurrentIndex(idx);
        }
    }
    hostEdit_->setText(f.host);
    portEdit_->setText(f.port);
    databaseEdit_->setText(f.database);
    usernameEdit_->setText(f.username);
    fileEdit_->setText(f.filePath);
    tlsCheck_->setChecked(f.tls);
    // A password lifted out of a pasted URL goes straight into the secure field;
    // it is never rendered back into the visible URL.
    if (!f.password.isEmpty()) {
        passwordEdit_->setText(f.password);
    }
}

ConnectionDialog::Fields ConnectionDialog::fieldsFromUi() const {
    Fields f;
    f.engineId = engineBox_->currentData().toString();
    f.host = hostEdit_->text();
    f.port = portEdit_->text();
    f.database = databaseEdit_->text();
    f.username = usernameEdit_->text();
    f.password = passwordEdit_->text();
    f.filePath = fileEdit_->text();
    f.tls = tlsCheck_->isChecked();
    return f;
}

void ConnectionDialog::renderUrlFromFields() {
    syncing_ = true;
    urlEdit_->setText(buildUrl(fieldsFromUi(), /*includePassword=*/false));
    urlEdit_->setCursorPosition(0);  // setText scrolls to the tail
    syncing_ = false;
}

// ---------------------------------------------------------------------------
// Slots
// ---------------------------------------------------------------------------

void ConnectionDialog::onEngineChanged(int /*index*/) {
    const Engine* e = currentEngine();
    if (e != nullptr) {
        reshapeForEngine(*e);
    }
    if (!syncing_) {
        renderUrlFromFields();
    }
}

void ConnectionDialog::onFieldEdited() {
    if (syncing_) {
        return;
    }
    renderUrlFromFields();
}

void ConnectionDialog::onUrlEdited() {
    if (syncing_) {
        return;
    }
    const Fields f = parseUrl(urlEdit_->text());
    if (f.engineId.isEmpty()) {
        return;  // half-typed / unknown scheme — keep the current engine + fields
    }
    syncing_ = true;
    applyFieldsToUi(f);
    // If the pasted URL carried a password we lifted it into the secure field
    // and must re-render the URL without it so the visible box never shows it.
    if (!f.password.isEmpty()) {
        urlEdit_->setText(buildUrl(fieldsFromUi(), /*includePassword=*/false));
    }
    syncing_ = false;
}

void ConnectionDialog::onReadOnlyToggled(bool on) {
    // A confirm-before-write prompt is redundant while writes are refused outright.
    confirmWritesCheck_->setEnabled(!on);
    if (on) {
        confirmWritesCheck_->setToolTip(
            QStringLiteral("Not needed while read-only is on — writes are refused."));
    } else {
        confirmWritesCheck_->setToolTip(
            QStringLiteral("Show a confirmation before INSERT / UPDATE / DELETE / DROP."));
    }
}

void ConnectionDialog::onBrowseFile() {
    const QString path = QFileDialog::getOpenFileName(
        this, QStringLiteral("Choose a SQLite database file"));
    if (!path.isEmpty()) {
        fileEdit_->setText(path);  // triggers onFieldEdited -> URL re-render
    }
}

void ConnectionDialog::onCheckEnforcement() {
    if (core_ == nullptr || originalName_.isEmpty()) {
        return;
    }
    enforcementLabel_->show();
    QString json;
    try {
        json = QString::fromStdString(core_->connectionInfoJson(originalName_.toStdString()));
    } catch (const dg::Error& e) {
        enforcementLabel_->setStyleSheet(QStringLiteral("color: #c0392b;"));
        enforcementLabel_->setText(QString::fromUtf8(e.what()));
        return;
    }
    const QJsonObject o = QJsonDocument::fromJson(json.toUtf8()).object();
    const QJsonValue ro = o.value(QStringLiteral("read_only"));
    if (ro.isNull() || ro.isUndefined()) {
        enforcementLabel_->setStyleSheet(QStringLiteral("color: gray;"));
        enforcementLabel_->setText(
            QStringLiteral("This connection is writeable — no read-only protection "
                           "is in force."));
        return;
    }
    const QJsonObject roo = ro.toObject();
    const QString level = roo.value(QStringLiteral("enforcement")).toString();
    const bool confirmed = roo.value(QStringLiteral("server_confirmed")).toBool(false);
    QString text;
    if (level == QStringLiteral("server")) {
        enforcementLabel_->setStyleSheet(QStringLiteral("color: #1e8449;"));
        text = confirmed
                   ? QStringLiteral("Read-only enforced by the server — the engine "
                                    "opened this session read-only and will refuse a "
                                    "write itself.")
                   : QStringLiteral("Read-only reported by the server, but not yet "
                                    "confirmed on a live session.");
    } else if (level == QStringLiteral("client")) {
        enforcementLabel_->setStyleSheet(QStringLiteral("color: #b9770e;"));
        text = QStringLiteral(
            "Read-only blocked by datagrep only — statements classified as writes "
            "are refused before dispatch. The server would still accept a write "
            "from anything that bypasses datagrep.");
    } else {
        enforcementLabel_->setStyleSheet(QStringLiteral("color: #c0392b;"));
        text = QStringLiteral(
            "No read-only enforcement is available for this engine — datagrep can "
            "refuse writes it sends, but nothing else is protected.");
    }
    enforcementLabel_->setText(text);
}

void ConnectionDialog::onTestConnection() {
    if (core_ == nullptr || testWatcher_->isRunning()) {
        return;
    }
    const QString urlWithPassword = buildUrl(fieldsFromUi(), true).trimmed();
    const bool unchanged = editing_ && haveOriginal_ &&
                           passwordEdit_->text().isEmpty() &&
                           urlEdit_->text().trimmed() == originalUrlNoPassword_;
    const QString name = unchanged ? originalName_ : QString();
    if (name.isEmpty() && urlWithPassword.isEmpty()) {
        testResultLabel_->setStyleSheet(QStringLiteral("color: gray;"));
        testResultLabel_->setText(
            QStringLiteral("Complete the connection details first."));
        testResultLabel_->show();
        return;
    }

    testButton_->setEnabled(false);
    testResultLabel_->setStyleSheet(QStringLiteral("color: gray;"));
    testResultLabel_->setText(QStringLiteral("Connecting…"));
    testResultLabel_->show();

    dg::Core* core = core_;
    testWatcher_->setFuture(QtConcurrent::run(
        [core, name = name.toStdString(),
         url = urlWithPassword.toStdString()]() -> QPair<QString, QString> {
            try {
                return {QString::fromStdString(core->connectionTestJson(name, url)),
                        QString()};
            } catch (const dg::Error& e) {
                return {QString(), QString::fromUtf8(e.what())};
            }
        }));
}

void ConnectionDialog::onTestFinished() {
    testButton_->setEnabled(true);
    const QPair<QString, QString> outcome = testWatcher_->result();
    if (!outcome.second.isEmpty()) {
        testResultLabel_->setStyleSheet(QStringLiteral("color: #c0392b;"));
        testResultLabel_->setText(
            QStringLiteral("Could not connect: %1").arg(outcome.second));
        return;
    }
    const QJsonObject o = QJsonDocument::fromJson(outcome.first.toUtf8()).object();
    const QString driver = o.value(QStringLiteral("driver")).toString();
    const QString product = o.value(QStringLiteral("product")).toString();
    const QString version = o.value(QStringLiteral("version")).toString();
    const quint64 elapsed =
        o.value(QStringLiteral("elapsed_ms")).toVariant().toULongLong();

    QString what = product.isEmpty() ? dg::engineDisplayName(driver) : product;
    if (!version.isEmpty() && version.toLower() != QStringLiteral("unknown")) {
        what += QLatin1Char(' ') + version;
    }
    QStringList lines;
    lines << QStringLiteral("Connected to %1 in %2 ms").arg(what).arg(elapsed);
    QStringList detailParts;
    for (const QJsonValue& v : o.value(QStringLiteral("details")).toArray()) {
        const QJsonArray pair = v.toArray();
        if (pair.size() == 2) {
            detailParts << QStringLiteral("%1: %2").arg(pair.at(0).toString(),
                                                        pair.at(1).toString());
        }
    }
    lines << (detailParts.isEmpty()
                  ? QStringLiteral("The engine accepted the connection and it was "
                                   "closed again — nothing was saved by testing.")
                  : detailParts.join(QStringLiteral(" · ")));
    testResultLabel_->setStyleSheet(QStringLiteral("color: #1e8449;"));
    testResultLabel_->setText(lines.join(QLatin1Char('\n')));
}

// ---------------------------------------------------------------------------
// Seeding the Edit form
// ---------------------------------------------------------------------------

void ConnectionDialog::seedForEdit(const QString& name) {
    nameEdit_->setText(name);
    // Baseline the diff against the form's current default state, so that if the
    // seed read fails (or a key is absent) the patch does not manufacture a
    // spurious change against an empty original.
    origColor_ = colorBox_->currentData().toString();
    if (core_ == nullptr) {
        return;
    }
    QString json;
    try {
        json = QString::fromStdString(core_->profileGetJson(name.toStdString()));
    } catch (const dg::Error& e) {
        showError(QStringLiteral("Could not read this connection back: %1")
                      .arg(QString::fromUtf8(e.what())));
        // The engine reshape still needs doing so the form is usable.
        onEngineChanged(engineBox_->currentIndex());
        return;
    }
    const QJsonObject o = QJsonDocument::fromJson(json.toUtf8()).object();

    const QString driver = o.value(QStringLiteral("driver")).toString();
    origReadOnly_ = o.value(QStringLiteral("read_only")).toBool(false);
    origConfirmWrites_ = o.value(QStringLiteral("confirm_writes")).toBool(false);
    origColor_ = o.value(QStringLiteral("color")).toString();

    const QJsonValue autoLimit = o.value(QStringLiteral("auto_limit"));
    origAutoLimit_ = (autoLimit.isDouble())
                         ? QString::number(static_cast<qlonglong>(autoLimit.toDouble()))
                         : QString();
    const QJsonValue idle = o.value(QStringLiteral("idle_timeout_s"));
    origIdleTimeout_ = (idle.isDouble())
                           ? QString::number(static_cast<qlonglong>(idle.toDouble()))
                           : QString();

    const bool hasSecret = o.value(QStringLiteral("has_secret")).toBool(false);

    // Rebuild the structured fields from the parsed config the ABI reports (it
    // returns no `url` key at all — config is the only route back).
    Fields f;
    const QJsonValue config = o.value(QStringLiteral("config"));
    if (config.isObject()) {
        f = fieldsFromConfig(driver, config.toObject());
    } else {
        const Engine* e = engineById(driver);
        if (e != nullptr) {
            f.engineId = e->id;
        }
    }

    syncing_ = true;
    applyFieldsToUi(f);
    // Engine reshape (applyFieldsToUi set the engine box, but if config was empty
    // we still owe a reshape for the driver).
    const Engine* e = currentEngine();
    if (e != nullptr) {
        reshapeForEngine(*e);
    }
    colorBox_->setCurrentIndex(qMax(0, colorBox_->findData(origColor_)));
    readOnlyCheck_->setChecked(origReadOnly_);
    confirmWritesCheck_->setChecked(origConfirmWrites_);
    autoLimitEdit_->setText(origAutoLimit_);
    idleTimeoutEdit_->setText(origIdleTimeout_);
    onReadOnlyToggled(origReadOnly_);
    // Now that the fields are in place, render the baseline URL.
    urlEdit_->setText(buildUrl(fieldsFromUi(), /*includePassword=*/false));
    urlEdit_->setCursorPosition(0);
    syncing_ = false;

    originalUrlNoPassword_ = urlEdit_->text();
    haveOriginal_ = true;

    if (hasSecret) {
        passwordEdit_->setPlaceholderText(
            QString::fromUtf8("••••••••"));
        passwordHint_->setText(QStringLiteral(
            "A password is saved in the system keychain. Leave this blank to keep "
            "it — datagrep never reads it back into the window."));
    }
}

// ---------------------------------------------------------------------------
// Building the payloads
// ---------------------------------------------------------------------------

QString ConnectionDialog::optionsJson() const {
    QJsonObject o;
    o.insert(QStringLiteral("read_only"), readOnlyCheck_->isChecked());
    o.insert(QStringLiteral("confirm_writes"), confirmWritesCheck_->isChecked());

    bool ok = false;
    const qlonglong lim = autoLimitEdit_->text().trimmed().toLongLong(&ok);
    if (ok && !autoLimitEdit_->text().trimmed().isEmpty()) {
        o.insert(QStringLiteral("auto_limit"), lim);
    }
    ok = false;
    const qlonglong idle = idleTimeoutEdit_->text().trimmed().toLongLong(&ok);
    if (ok && !idleTimeoutEdit_->text().trimmed().isEmpty()) {
        o.insert(QStringLiteral("idle_timeout_s"), idle);
    }
    const QString color = colorBox_->currentData().toString();
    if (!color.isEmpty()) {
        o.insert(QStringLiteral("color"), color);
    }
    return QString::fromUtf8(QJsonDocument(o).toJson(QJsonDocument::Compact));
}

QString ConnectionDialog::patchJson() const {
    // ONLY the keys that actually moved. A full-object write would round-trip
    // fields this build does not understand and silently reset them. Absent key
    // = leave alone; JSON null = clear (auto_limit / idle_timeout_s / color only).
    QJsonObject p;

    if (nameEdit_->text().trimmed() != originalName_) {
        p.insert(QStringLiteral("name"), nameEdit_->text().trimmed());
    }

    // url: send it when the user typed a password (so it reaches the keychain)
    // or the connection itself changed. With neither, the stored secret and URL
    // are left untouched.
    const Fields f = fieldsFromUi();
    const QString currentUrl = buildUrl(f, /*includePassword=*/false);
    const bool typedPassword = !passwordEdit_->text().isEmpty();
    const bool urlChanged =
        haveOriginal_ && currentUrl != originalUrlNoPassword_ && !currentUrl.isEmpty();
    if (typedPassword || urlChanged) {
        p.insert(QStringLiteral("url"),
                 typedPassword ? buildUrl(f, /*includePassword=*/true) : currentUrl);
    }

    if (readOnlyCheck_->isChecked() != origReadOnly_) {
        p.insert(QStringLiteral("read_only"), readOnlyCheck_->isChecked());
    }
    if (confirmWritesCheck_->isChecked() != origConfirmWrites_) {
        p.insert(QStringLiteral("confirm_writes"), confirmWritesCheck_->isChecked());
    }

    const QString color = colorBox_->currentData().toString();
    if (color != origColor_) {
        // Clearing a colour is JSON null, not an absent key.
        if (color.isEmpty()) {
            p.insert(QStringLiteral("color"), QJsonValue(QJsonValue::Null));
        } else {
            p.insert(QStringLiteral("color"), color);
        }
    }

    const QString autoLimit = autoLimitEdit_->text().trimmed();
    if (autoLimit != origAutoLimit_) {
        if (autoLimit.isEmpty()) {
            p.insert(QStringLiteral("auto_limit"), QJsonValue(QJsonValue::Null));
        } else {
            p.insert(QStringLiteral("auto_limit"), autoLimit.toLongLong());
        }
    }
    const QString idle = idleTimeoutEdit_->text().trimmed();
    if (idle != origIdleTimeout_) {
        if (idle.isEmpty()) {
            p.insert(QStringLiteral("idle_timeout_s"), QJsonValue(QJsonValue::Null));
        } else {
            p.insert(QStringLiteral("idle_timeout_s"), idle.toLongLong());
        }
    }

    return QString::fromUtf8(QJsonDocument(p).toJson(QJsonDocument::Compact));
}

void ConnectionDialog::onAccept() {
    if (core_ == nullptr) {
        return;
    }
    const QString name = nameEdit_->text().trimmed();
    if (name.isEmpty()) {
        showError(QStringLiteral("A name is required."));
        return;
    }

    if (!editing_) {
        const QString url = buildUrl(fieldsFromUi(), /*includePassword=*/true);
        if (url.isEmpty()) {
            showError(QStringLiteral("A host (or, for SQLite, a file) is required."));
            return;
        }
        try {
            core_->addProfileJson(name.toStdString(), url.toStdString(),
                                  optionsJson().toStdString());
        } catch (const dg::Error& e) {
            showError(QString::fromUtf8(e.what()));
            return;
        }
        savedName_ = name;
        accept();
        return;
    }

    // Editing: a minimal patch. Nothing changed => nothing to write.
    const QString patch = patchJson();
    const QJsonObject po = QJsonDocument::fromJson(patch.toUtf8()).object();
    if (po.isEmpty()) {
        savedName_ = originalName_;
        accept();
        return;
    }
    try {
        core_->updateProfile(originalName_.toStdString(), patch.toStdString());
    } catch (const dg::Error& e) {
        showError(QString::fromUtf8(e.what()));
        return;
    }
    savedName_ = name;  // the patch renamed it if name changed
    accept();
}
