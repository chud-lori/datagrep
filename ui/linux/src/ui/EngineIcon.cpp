#include "EngineIcon.hpp"

#include <QFile>
#include <QGuiApplication>
#include <QHash>
#include <QIconEngine>
#include <QPainter>
#include <QPainterPath>
#include <QPalette>
#include <QPixmap>
#include <QSvgRenderer>

#include <utility>

namespace dg {
namespace {

struct EngineArt {
    const char* file;
    const char* lightFill;
    const char* darkFill;
    QColor tint;
};

const EngineArt* artFor(const QString& key) {
    static const struct {
        const char* id;
        EngineArt art;
    } table[] = {
        {"postgres", {"postgresql", "#4169E1", "#7D9EF5", QColor()}},
        {"mysql", {"mysql", "#4479A1", "#7FB3D5", QColor()}},
        {"sqlite", {"sqlite", "#003B57", "#4D9BC4", QColor()}},
        {"redis", {"redis", "#FF4438", "#FF6B5E", QColor()}},
        {"mongo", {"mongodb", "#47A248", "#6FD070", QColor()}},
        {"elasticsearch", {nullptr, nullptr, nullptr, QColor(0x00, 0xBF, 0xB3)}},
    };
    for (const auto& row : table) {
        if (key == QLatin1String(row.id)) {
            return &row.art;
        }
    }
    return nullptr;
}

bool darkSurface() {
    return QGuiApplication::palette().color(QPalette::Window).lightness() < 128;
}

QByteArray svgBytes(const EngineArt& art, bool dark) {
    QFile f(QStringLiteral(":/engines/") + QLatin1String(art.file) +
            QStringLiteral(".svg"));
    if (!f.open(QIODevice::ReadOnly)) {
        return {};
    }
    QByteArray svg = f.readAll();
    if (dark) {
        svg.replace(QByteArray("fill=\"") + art.lightFill + '"',
                    QByteArray("fill=\"") + art.darkFill + '"');
    }
    return svg;
}

void drawMagnifier(QPainter& p, const QRectF& r, const QColor& tint) {
    const qreal s = r.width();
    QPen pen(tint, s * 0.12, Qt::SolidLine, Qt::RoundCap);
    p.setPen(pen);
    p.setBrush(Qt::NoBrush);
    const qreal radius = s * 0.28;
    p.drawEllipse(QPointF(r.left() + s * 0.42, r.top() + s * 0.42), radius,
                  radius);
    p.drawLine(QPointF(r.left() + s * 0.64, r.top() + s * 0.64),
               QPointF(r.left() + s * 0.88, r.top() + s * 0.88));
}

void drawCylinder(QPainter& p, const QRectF& r) {
    QColor tint = QGuiApplication::palette().color(QPalette::Text);
    tint.setAlpha(150);
    const qreal s = r.width();
    const qreal rx = s * 0.30;
    const qreal ry = s * 0.12;
    const qreal cx = r.left() + s * 0.5;
    const qreal top = r.top() + s * 0.18;
    const qreal bottom = r.top() + s * 0.82;
    QPainterPath path;
    path.addEllipse(QPointF(cx, top), rx, ry);
    path.addRect(cx - rx, top, rx * 2, bottom - top);
    path.addEllipse(QPointF(cx, bottom), rx, ry);
    p.setPen(Qt::NoPen);
    p.setBrush(tint);
    p.drawPath(path.simplified());
}

QPixmap renderPixmap(const QString& key, const QColor& marker, const QSize& size,
                     qreal scale) {
    if (size.isEmpty() || scale <= 0.0) {
        return QPixmap();
    }
    const bool dark = darkSurface();
    const QString cacheKey = QStringLiteral("%1|%2|%3|%4x%5|%6")
                                 .arg(key, dark ? QStringLiteral("d")
                                                : QStringLiteral("l"),
                                      marker.isValid() ? marker.name()
                                                       : QStringLiteral("-"))
                                 .arg(size.width())
                                 .arg(size.height())
                                 .arg(scale);
    static QHash<QString, QPixmap> cache;
    const auto it = cache.constFind(cacheKey);
    if (it != cache.constEnd()) {
        return it.value();
    }

    QPixmap pm(size * scale);
    pm.setDevicePixelRatio(scale);
    pm.fill(Qt::transparent);
    QPainter p(&pm);
    p.setRenderHint(QPainter::Antialiasing);

    const qreal w = size.width();
    const qreal h = size.height();
    const qreal s = qMin(w, h);
    // Mark right-aligned in its square; the leading edge belongs to the bar.
    const QRectF artRect(w - s, (h - s) / 2.0, s, s);

    const EngineArt* art = artFor(key);
    bool drewArtwork = false;
    if (art != nullptr && art->file != nullptr) {
        QSvgRenderer renderer(svgBytes(*art, dark));
        if (renderer.isValid()) {
            renderer.render(&p, artRect);
            drewArtwork = true;
        }
    }
    if (!drewArtwork) {
        if (art != nullptr && art->tint.isValid()) {
            drawMagnifier(p, artRect, art->tint);
        } else {
            drawCylinder(p, artRect);
        }
    }

    const qreal barW = qMax<qreal>(2.0, s * 0.1875);
    if (marker.isValid() && w - s >= barW + 1.0) {
        p.setPen(Qt::NoPen);
        p.setBrush(marker);
        p.drawRoundedRect(QRectF(0.0, (h - s * 0.94) / 2.0, barW, s * 0.94),
                          1.5, 1.5);
    }
    p.end();

    cache.insert(cacheKey, pm);
    return pm;
}

class EngineIconEngine : public QIconEngine {
public:
    EngineIconEngine(QString key, QColor marker)
        : key_(std::move(key)), marker_(std::move(marker)) {}

    void paint(QPainter* painter, const QRect& rect, QIcon::Mode mode,
               QIcon::State state) override {
        const qreal scale = painter->device() != nullptr
                                ? painter->device()->devicePixelRatio()
                                : 1.0;
        painter->drawPixmap(rect, scaledPixmap(rect.size(), mode, state, scale));
    }

    QPixmap pixmap(const QSize& size, QIcon::Mode mode,
                   QIcon::State state) override {
        return scaledPixmap(size, mode, state, 1.0);
    }

    QPixmap scaledPixmap(const QSize& size, QIcon::Mode /*mode*/,
                         QIcon::State /*state*/, qreal scale) override {
        return renderPixmap(key_, marker_, size, scale);
    }

    QIconEngine* clone() const override {
        return new EngineIconEngine(key_, marker_);
    }

private:
    QString key_;
    QColor marker_;
};

}  // namespace

QString canonicalDriverId(const QString& id) {
    const QString s = id.toLower();
    if (s.startsWith(QStringLiteral("postgres")) || s == QStringLiteral("pg") ||
        s == QStringLiteral("psql")) {
        return QStringLiteral("postgres");
    }
    if (s.startsWith(QStringLiteral("mysql")) ||
        s.startsWith(QStringLiteral("maria"))) {
        return QStringLiteral("mysql");
    }
    if (s.startsWith(QStringLiteral("sqlite"))) {
        return QStringLiteral("sqlite");
    }
    if (s.startsWith(QStringLiteral("redis"))) {
        return QStringLiteral("redis");
    }
    if (s.startsWith(QStringLiteral("mongo"))) {
        return QStringLiteral("mongo");
    }
    if (s.startsWith(QStringLiteral("elastic")) ||
        s.startsWith(QStringLiteral("opensearch"))) {
        return QStringLiteral("elasticsearch");
    }
    return s;
}

QString engineDisplayName(const QString& driverId) {
    const QString key = canonicalDriverId(driverId);
    if (key == QStringLiteral("postgres")) return QStringLiteral("PostgreSQL");
    if (key == QStringLiteral("mysql")) return QStringLiteral("MySQL");
    if (key == QStringLiteral("sqlite")) return QStringLiteral("SQLite");
    if (key == QStringLiteral("redis")) return QStringLiteral("Redis");
    if (key == QStringLiteral("mongo")) return QStringLiteral("MongoDB");
    if (key == QStringLiteral("elasticsearch")) {
        return QStringLiteral("Elasticsearch");
    }
    return driverId;
}

QIcon engineIcon(const QString& driverId, const QColor& marker) {
    return QIcon(new EngineIconEngine(canonicalDriverId(driverId), marker));
}

}  // namespace dg
