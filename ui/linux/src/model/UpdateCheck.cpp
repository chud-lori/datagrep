#include "UpdateCheck.hpp"

#include <QJsonDocument>
#include <QJsonObject>
#include <QNetworkAccessManager>
#include <QNetworkReply>
#include <QNetworkRequest>
#include <QSettings>
#include <QStringList>

#include <array>
#include <optional>

namespace {

QUrl manifestUrl() {
    return QUrl(QStringLiteral("https://chud-lori.github.io/datagrep/latest.json"));
}

QString checkOnLaunchKey() { return QStringLiteral("updateCheckOnLaunch"); }
QString skippedVersionKey() { return QStringLiteral("updateSkippedVersion"); }

QString skippedVersion() {
    return QSettings().value(skippedVersionKey()).toString();
}

}  // namespace

UpdateCheck::UpdateCheck(QObject* parent)
    : QObject(parent), network_(new QNetworkAccessManager(this)) {}

QString UpdateCheck::currentVersion() {
    // Defined by CMake from the workspace manifest — the same source the
    // packaging scripts stamp on the artifacts.
    return QStringLiteral(DATAGREP_APP_VERSION);
}

bool UpdateCheck::checkOnLaunchEnabled() {
    return QSettings().value(checkOnLaunchKey(), true).toBool();
}

void UpdateCheck::setCheckOnLaunchEnabled(bool enabled) {
    QSettings().setValue(checkOnLaunchKey(), enabled);
}

void UpdateCheck::checkOnLaunchIfEnabled() {
    if (!checkOnLaunchEnabled() || didCheckThisLaunch_) {
        return;
    }
    didCheckThisLaunch_ = true;
    fetchManifest(/*userInitiated=*/false);
}

void UpdateCheck::checkNow() {
    fetchManifest(/*userInitiated=*/true);
}

void UpdateCheck::skip(const dg::UpdateManifest& manifest) {
    QSettings().setValue(skippedVersionKey(), manifest.version);
}

// One GET, short timeout, default manager — nothing cached, nothing persisted.
void UpdateCheck::fetchManifest(bool userInitiated) {
    QNetworkRequest request(manifestUrl());
    request.setRawHeader("Accept", "application/json");
    request.setHeader(QNetworkRequest::UserAgentHeader,
                      QStringLiteral("datagrep/%1").arg(currentVersion()));
    request.setTransferTimeout(10000);

    QNetworkReply* reply = network_->get(request);
    connect(reply, &QNetworkReply::finished, this, [this, reply, userInitiated]() {
        reply->deleteLater();

        std::optional<dg::UpdateManifest> manifest;
        const int status =
            reply->attribute(QNetworkRequest::HttpStatusCodeAttribute).toInt();
        if (reply->error() == QNetworkReply::NoError && status >= 200 &&
            status < 300) {
            const QJsonObject o =
                QJsonDocument::fromJson(reply->readAll()).object();
            dg::UpdateManifest m;
            m.version = o.value(QStringLiteral("version")).toString();
            m.tag = o.value(QStringLiteral("tag")).toString();
            m.releaseUrl = QUrl(o.value(QStringLiteral("release_url")).toString());
            if (!m.version.isEmpty() && !m.tag.isEmpty()) {
                manifest = m;
            }
        }

        if (!manifest.has_value()) {
            if (userInitiated) {
                emit checkFinished(false, true);
            }
            return;  // silence on any launch-check failure
        }
        const bool newer = isNewer(manifest->version, currentVersion());
        if (!userInitiated) {
            if (newer &&
                normalize(skippedVersion()) != normalize(manifest->version)) {
                emit updateAvailable(*manifest);
            }
            return;
        }
        if (newer) {
            emit updateAvailable(*manifest);
        }
        emit checkFinished(newer, false);
    });
}

QString UpdateCheck::normalize(const QString& v) {
    return v.startsWith(QLatin1Char('v')) ? v.mid(1) : v;
}

bool UpdateCheck::isNewer(const QString& a, const QString& b) {
    const auto parse = [](const QString& s) {
        std::array<quint64, 3> out{0, 0, 0};
        const QStringList parts = normalize(s).split(QLatin1Char('.'));
        for (int i = 0; i < 3 && i < parts.size(); ++i) {
            QString digits;
            for (const QChar c : parts.at(i)) {
                if (!c.isDigit()) {
                    break;
                }
                digits.append(c);
            }
            out[static_cast<std::size_t>(i)] = digits.toULongLong();
        }
        return out;
    };
    const auto x = parse(a);
    const auto y = parse(b);
    if (x[0] != y[0]) {
        return x[0] > y[0];
    }
    if (x[1] != y[1]) {
        return x[1] > y[1];
    }
    return x[2] > y[2];
}
