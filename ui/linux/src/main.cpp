// datagrep Linux UI — entry point.
//
// A native Qt6/C++ front-end over the same Rust core the macOS app uses, linked
// through the frozen C ABI (crates/datagrep-ffi/include/datagrep.h). All engine
// work happens on the core's own tokio runtime thread; this process is a pure
// CoreApi client and holds no business logic.

#include "ui/MainWindow.hpp"

#include <QApplication>

int main(int argc, char** argv) {
    QApplication app(argc, argv);
    QApplication::setApplicationName(QStringLiteral("datagrep"));
    QApplication::setOrganizationName(QStringLiteral("datagrep"));

    MainWindow window;
    window.show();
    return QApplication::exec();
}
