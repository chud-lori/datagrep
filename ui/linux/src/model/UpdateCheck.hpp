// UpdateCheck.hpp — the once-per-launch release check, over the same static
// manifest the macOS app reads. Its contract is deliberate and shared:
//
//   1. ONE silent GET per launch — no timer, no retry, no polling.
//   2. Never downloads, never installs. It links to the release; the user
//      decides. Package-manager installs included: the notice is information,
//      so it cannot fight dpkg/rpm, and no repository exists that would make
//      those installs self-update anyway.
//   3. Opt-out (`updateCheckOnLaunch`), and the GET is all it ever sends.
//   4. Fails silently — a network error must never become a dialog.

#ifndef DATAGREP_UPDATE_CHECK_HPP
#define DATAGREP_UPDATE_CHECK_HPP

#include <QObject>
#include <QString>
#include <QUrl>

class QNetworkAccessManager;

namespace dg {

// Shape of `latest.json` (docs/latest.json in the repo). Extra keys ignored.
struct UpdateManifest {
    QString version;
    QString tag;
    QUrl releaseUrl;
};

}  // namespace dg

class UpdateCheck : public QObject {
    Q_OBJECT

public:
    explicit UpdateCheck(QObject* parent = nullptr);

    // The workspace version baked in at build time; the release script bumps
    // it together with the manifest.
    static QString currentVersion();

    // Preference keys match the macOS defaults names, so the documented
    // opt-out story reads the same on both platforms.
    static bool checkOnLaunchEnabled();
    static void setCheckOnLaunchEnabled(bool enabled);

    // The launch check. Later calls are no-ops, so callers need not guard
    // re-entry. Emits updateAvailable() only for a strictly newer, un-skipped
    // release; every failure is silence.
    void checkOnLaunchIfEnabled();

    // Explicit user-initiated check. Ignores the skip list and the
    // once-per-launch guard, and reports the outcome — the user is watching.
    void checkNow();

    // "Skip this version": suppresses the launch notice for exactly this
    // version — a newer release notifies again.
    void skip(const dg::UpdateManifest& manifest);

    static QString normalize(const QString& v);
    // Strictly newer. Unparseable components count as 0 and pre-release
    // suffixes are stripped — this never gates anything security-relevant.
    static bool isNewer(const QString& a, const QString& b);

signals:
    void updateAvailable(const dg::UpdateManifest& manifest);
    // checkNow()'s outcome, including "nothing newer" and "check failed".
    void checkFinished(bool newerFound, bool failed);

private:
    void fetchManifest(bool userInitiated);

    QNetworkAccessManager* network_;
    bool didCheckThisLaunch_ = false;
};

#endif  // DATAGREP_UPDATE_CHECK_HPP
