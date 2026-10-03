#include "ui/Appearance.hpp"
#include "ui/MainWindow.hpp"
#include "ui/Theme.hpp"

#include <QApplication>

int main(int argc, char** argv) {
    QApplication app(argc, argv);
    QApplication::setApplicationName(QStringLiteral("datagrep"));
    QApplication::setOrganizationName(QStringLiteral("datagrep"));
    QApplication::setApplicationVersion(QStringLiteral(DATAGREP_APP_VERSION));

    // Theme, then the stored light/dark palette, must both land before the window polishes.
    dg::applyTheme();
    Appearance::instance().applyStored();

    MainWindow window;
    window.show();
    return QApplication::exec();
}
