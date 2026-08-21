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

    // Baked in at build time; the release script bumps it with the manifest.
    static QString currentVersion();

    // Preference keys match the macOS defaults names.
    static bool checkOnLaunchEnabled();
    static void setCheckOnLaunchEnabled(bool enabled);

    void checkOnLaunchIfEnabled();

    void checkNow();

    // Suppresses the launch notice for exactly this version.
    void skip(const dg::UpdateManifest& manifest);

    static QString normalize(const QString& v);
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
